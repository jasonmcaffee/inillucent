//! The read families: `select`, `join`, `compound`, `cte`, `subquery`,
//! `expression` and `function`.
//!
//! Invariant: **every read family takes the same six shared axes (source,
//! access, placement, data, affinity, binding) and adds only the axes of its
//! own construct, so a join and a window function meet every source kind and
//! every placement by the same rules.** Each family's builder turns one row
//! into one query over a [`Relation`]; [`read_case`] puts it in its scene, at
//! its placement, and adds the Layer 3 wrappings and properties.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::frame::{
    constant, layer_three, place, placement_allowed, Constant, Query, BINDINGS, PLACEMENTS,
};
use crate::statement_matrix::templates::scene::{
    relation, scene_allowed, Relation, ACCESS, AFFINITIES, DATA, SOURCES,
};
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The shared axes, in the order every read family lists them.
pub fn shared_axes() -> Vec<Axis> {
    vec![
        Axis::new("source", SOURCES),
        Axis::new("access", ACCESS),
        Axis::new("placement", PLACEMENTS),
        Axis::new("data", DATA),
        Axis::new("affinity", AFFINITIES),
        Axis::new("binding", BINDINGS),
    ]
}

/// The shared constraint.
///
/// @param pick - the partial row
pub fn shared_allowed(pick: &Pick) -> bool {
    scene_allowed(pick.get("source"), pick.get("access"), pick.get("data"))
        && placement_allowed(
            pick.get("placement"),
            pick.get("source"),
            pick.get("binding"),
        )
}

/// The predicates a read filters with, over the column under test.
const PREDICATES: &[&str] = &[
    "gt", "eq", "null", "between", "in", "like", "collate", "expr", "and_b", "not",
];

/// Writes a predicate.
///
/// @param kind - the predicate axis value
/// @param relation - the scene
/// @param c - the constant, as the binding writes it
fn predicate(kind: &str, relation: &Relation, c: &str) -> String {
    let (a, b) = (&relation.a, &relation.b);
    match kind {
        "eq" => format!("{a} = {c}"),
        "null" => format!("{a} IS NULL OR {a} = {c}"),
        "between" => format!("{a} BETWEEN {c} AND {c} + 2"),
        "in" => format!("{a} IN ({c}, 3, 'abc')"),
        "like" => format!("{a} LIKE 'a%' OR {a} = {c}"),
        "collate" => format!("{a} = 'abc' COLLATE NOCASE OR {a} = {c}"),
        "expr" => format!("coalesce({a}, 0) >= {c}"),
        "and_b" => format!("{b} IS NOT NULL AND {a} < {c}"),
        "not" => format!("NOT ({a} > {c})"),
        _ => format!("{a} > {c}"),
    }
}

/// Builds a read case from a query the family made.
///
/// @param pick - the row
/// @param make - the family's query builder, given the scene and the constant
pub fn read_case(
    pick: &Pick,
    make: &dyn Fn(&Relation, &Constant) -> Option<Query>,
) -> Option<Case> {
    let source = pick.value("source");
    let access = pick.value("access");
    let scene = relation(source, access, pick.value("data"), pick.value("affinity"));
    let c = constant(pick.value("binding"), "2", "1");
    let query = make(&scene, &c)?;
    let mut case = Case::new("", "");
    case.setup = scene.setup.clone();
    case.records = scene.prelude.clone();
    let temporary = matches!(source, "temp" | "attached");
    place(&mut case, &query, pick.value("placement"), temporary);
    let indexed = !matches!(access, "none" | "not_indexed");
    layer_three(&mut case, &query, indexed);
    Some(case)
}

/// A plain query: the select list, the predicate, and what follows it.
///
/// @param scene - the relation
/// @param list - the result columns, named `c1` onwards
/// @param count - how many there are
/// @param predicate - the `WHERE` condition, or empty
/// @param tail - `GROUP BY`, `ORDER BY` or `LIMIT`, or empty
/// @param c - the constant, for its binds
fn plain(
    scene: &Relation,
    list: &str,
    count: usize,
    predicate: &str,
    tail: &str,
    c: &Constant,
) -> Query {
    let filter = if predicate.is_empty() {
        String::new()
    } else {
        format!("WHERE {predicate}")
    };
    Query {
        sql: scene.select(list, &format!("{filter} {tail}")),
        columns: count,
        sort: Sort::RowSort,
        binds: c.binds.clone(),
        relation: scene.clone(),
        select_list: format!("{} AS c1, {} AS c2", scene.k, scene.a),
        first: scene.k.clone(),
        predicate: predicate.to_string(),
        tail: tail.to_string(),
        checkable: true,
    }
}

/// Every read family.
pub fn families() -> Vec<Family> {
    vec![
        select(),
        join(),
        compound(),
        cte(),
        subquery(),
        expression(),
        function(),
    ]
}

/// A family from its construct axes and builder, with the shared axes added.
fn family(
    name: &'static str,
    own: Vec<Axis>,
    allowed: fn(&Pick) -> bool,
    build: fn(&Pick) -> Option<Case>,
) -> Family {
    let mut axes = own;
    axes.extend(shared_axes());
    Family {
        name,
        axes,
        allowed,
        build,
    }
}

/// `select`: the clauses of a `SELECT` core.
fn select() -> Family {
    family(
        "select",
        vec![
            Axis::new(
                "clause",
                &[
                    "where",
                    "distinct",
                    "group",
                    "order_nulls",
                    "limit",
                    "aggregate",
                    "alias_order",
                ],
            ),
            Axis::new("predicate", PREDICATES),
        ],
        shared_allowed,
        |pick| {
            read_case(pick, &|scene, c| {
                let p = predicate(pick.value("predicate"), scene, &c.text);
                let (k, a, b) = (&scene.k, &scene.a, &scene.b);
                Some(match pick.value("clause") {
                    "distinct" => plain(scene, &format!("DISTINCT {a} AS c1"), 1, &p, "", c),
                    "group" => plain(
                        scene,
                        &format!("{a} AS c1, count(*) AS c2"),
                        2,
                        &p,
                        &format!("GROUP BY {a} HAVING count(*) >= 1"),
                        c,
                    ),
                    "order_nulls" => Query {
                        sort: Sort::NoSort,
                        ..plain(
                            scene,
                            &format!("{a} AS c1, {k} AS c2"),
                            2,
                            &p,
                            &format!("ORDER BY {a} NULLS FIRST, {k}"),
                            c,
                        )
                    },
                    "limit" => plain(
                        scene,
                        &format!("{k} AS c1, {a} AS c2"),
                        2,
                        &p,
                        &format!("ORDER BY {k} LIMIT 3 OFFSET 1"),
                        c,
                    ),
                    "aggregate" => Query {
                        ..plain(
                            scene,
                            &format!(
                                "count(*) AS c1, count({a}) AS c2, min({a}) AS c3, max({a}) AS c4, \
                                 sum(CASE WHEN {a} > {} THEN 1 ELSE 0 END) AS c5",
                                c.text
                            ),
                            5,
                            "",
                            "",
                            c,
                        )
                    },
                    "alias_order" => Query {
                        sort: Sort::NoSort,
                        ..plain(
                            scene,
                            &format!("{k} AS c1, {b} AS a, {a} AS c3"),
                            3,
                            &p,
                            "ORDER BY a, c1, c3",
                            c,
                        )
                    },
                    _ => plain(scene, &format!("{k} AS c1, {a} AS c2"), 2, &p, "", c),
                })
            })
        },
    )
}

/// The name of the table a join, a compound or a subquery reads beside the
/// scene: `r`, or `r_<index>` when the case puts an index on it, so the
/// indexes of different cases never meet in one shared fixture.
fn other_name(index: &str) -> String {
    if index == "none" {
        "r".to_string()
    } else {
        format!("r_{index}")
    }
}

/// The table a join, a compound or a subquery reads beside the scene.
fn other_table(index: &str) -> Vec<Record> {
    let name = other_name(index);
    let mut out = vec![
        Record::ok(format!("CREATE TABLE {name}(k INTEGER, a, w TEXT)")),
        Record::ok(format!("INSERT INTO {name} VALUES (1, 1, 'x'), (2, 1, 'y'), (3, NULL, 'z'), (4, 5, 'w'), (5, 'abc', 'v')")),
    ];
    match index {
        "a" => out.push(Record::ok(format!("CREATE INDEX {name}_ix ON {name}(a)"))),
        "k" => out.push(Record::ok(format!("CREATE INDEX {name}_ix ON {name}(k)"))),
        "a_k" => out.push(Record::ok(format!(
            "CREATE INDEX {name}_ix ON {name}(a, k)"
        ))),
        "unique" => out.push(Record::ok(format!(
            "CREATE UNIQUE INDEX {name}_ix ON {name}(k)"
        ))),
        _ => {}
    }
    out
}

/// Adds the other table to a case's setup.
fn with_other(mut case: Case, index: &str) -> Case {
    let mut setup = other_table(index);
    setup.append(&mut case.setup);
    case.setup = setup;
    case
}

/// `join`: every join kind and constraint, with an index on part of the
/// constraint (the `LEFT JOIN` and `RIGHT JOIN` defects of section 1.1).
fn join() -> Family {
    family(
        "join",
        vec![
            Axis::new(
                "kind",
                &["comma", "inner", "cross", "left", "right", "full"],
            ),
            Axis::new("on", &["eq_a", "eq_a_and_k", "using", "natural"]),
            Axis::new("right_index", &["none", "a", "k", "a_k", "unique"]),
        ],
        |pick| {
            shared_allowed(pick)
                && !pick.forbids(
                    "on",
                    "source",
                    &[
                        ("using", "tvf"),
                        ("natural", "tvf"),
                        ("using", "fts5"),
                        ("natural", "fts5"),
                    ],
                )
                && !pick.forbids(
                    "kind",
                    "on",
                    &[
                        ("comma", "using"),
                        ("comma", "natural"),
                        ("cross", "using"),
                        ("cross", "natural"),
                    ],
                )
        },
        |pick| {
            let case = read_case(pick, &|scene, c| {
                let (k, a) = (&scene.k, &scene.a);
                let kind = pick.value("kind");
                let condition = match pick.value("on") {
                    "eq_a_and_k" => format!("r.a = {a} AND r.k > {}", c.text),
                    _ => format!("r.a = {a}"),
                };
                let right = format!("{} AS r", other_name(pick.value("right_index")));
                let (join, filter) = match (kind, pick.value("on")) {
                    ("comma", _) => (format!(", {right}"), format!("WHERE {condition}")),
                    ("cross", _) => (format!("CROSS JOIN {right}"), format!("WHERE {condition}")),
                    (_, "using") => (
                        format!("{} JOIN {right} USING (a)", word(kind)),
                        String::new(),
                    ),
                    (_, "natural") => (
                        format!("NATURAL {} JOIN {right}", word(kind)),
                        String::new(),
                    ),
                    _ => (
                        format!("{} JOIN {right} ON {condition}", word(kind)),
                        String::new(),
                    ),
                };
                let list = format!("{k} AS c1, {a} AS c2, r.k AS c3, r.w AS c4");
                Some(Query {
                    predicate: String::new(),
                    checkable: true,
                    ..plain(scene, &list, 4, "", &format!("{join} {filter}"), c)
                })
            })?;
            Some(with_other(case, pick.value("right_index")))
        },
    )
}

/// The keyword for a join kind.
fn word(kind: &str) -> &'static str {
    match kind {
        "left" => "LEFT",
        "right" => "RIGHT",
        "full" => "FULL",
        _ => "INNER",
    }
}

/// `compound`: the four operators, with and without an `ORDER BY` and
/// `LIMIT` on the whole.
fn compound() -> Family {
    family(
        "compound",
        vec![
            Axis::new("operator", &["UNION", "UNION ALL", "INTERSECT", "EXCEPT"]),
            Axis::new("arm", &["other_table", "same_source", "values"]),
            Axis::new("order", &["none", "order", "order_limit"]),
        ],
        // SQLite refuses an ORDER BY after a VALUES arm of several rows; the
        // Layer 1 case `compound-values-order` holds that refusal.
        |pick| {
            shared_allowed(pick)
                && !(pick.get("arm") == Some("values")
                    && pick.get("order").is_some_and(|order| order != "none"))
        },
        |pick| {
            let case = read_case(pick, &|scene, c| {
                let (k, a) = (&scene.k, &scene.a);
                let second = match pick.value("arm") {
                    "same_source" => {
                        format!("SELECT {a}, {k} FROM {} WHERE {a} IS NULL", scene.from)
                    }
                    "values" => format!("VALUES (1, 1), (2, {})", c.text),
                    _ => "SELECT r.a, r.k FROM r".to_string(),
                };
                let (tail, sort) = match pick.value("order") {
                    "order" => ("ORDER BY 1, 2", Sort::NoSort),
                    "order_limit" => ("ORDER BY 1, 2 LIMIT 3", Sort::NoSort),
                    _ => ("", Sort::RowSort),
                };
                let operator = pick.value("operator");
                let body = format!("WHERE {a} > {} {operator} {second} {tail}", c.text);
                Some(Query {
                    sort,
                    predicate: String::new(),
                    first: String::new(),
                    ..plain(scene, &format!("{a} AS c1, {k} AS c2"), 2, "", &body, c)
                })
            })?;
            Some(with_other(case, "none"))
        },
    )
}

/// The `WITH` clause for a query that adds its own CTEs to the scene's.
///
/// @param scene - the relation, which may already have a `WITH`
/// @param recursive - whether the clause needs `RECURSIVE`
/// @param extra - the CTEs to add, `name AS (...)`
fn with_clause(scene: &Relation, recursive: bool, extra: &str) -> String {
    let keyword = if recursive { "WITH RECURSIVE" } else { "WITH" };
    match scene.with.strip_prefix("WITH ") {
        Some(own) => format!("{keyword} {own}, {extra}"),
        None => format!("{keyword} {extra}"),
    }
}

/// `cte`: plain, recursive, materialized and not, and one used twice.
fn cte() -> Family {
    family(
        "cte",
        vec![
            Axis::new(
                "form",
                &[
                    "plain",
                    "recursive",
                    "materialized",
                    "not_materialized",
                    "twice",
                ],
            ),
            Axis::new("predicate", PREDICATES),
        ],
        shared_allowed,
        |pick| {
            read_case(pick, &|scene, c| {
                let (k, a) = (&scene.k, &scene.a);
                let p = predicate(pick.value("predicate"), scene, &c.text);
                let from = &scene.from;
                let body = format!("SELECT {k} AS c1, {a} AS c2 FROM {from} WHERE {p}");
                let sql = match pick.value("form") {
                    "recursive" => format!(
                        "{} SELECT n.x AS c1, q.c2 AS c2 FROM n JOIN q ON q.c1 = n.x",
                        with_clause(
                            scene,
                            true,
                            &format!("n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 6), q AS ({body})")
                        )
                    ),
                    "materialized" => format!(
                        "{} SELECT c1, c2 FROM q",
                        with_clause(scene, false, &format!("q AS MATERIALIZED ({body})"))
                    ),
                    "not_materialized" => format!(
                        "{} SELECT c1, c2 FROM q",
                        with_clause(scene, false, &format!("q AS NOT MATERIALIZED ({body})"))
                    ),
                    "twice" => format!(
                        "{} SELECT x.c1 AS c1, y.c2 AS c2 FROM q AS x JOIN q AS y ON x.c1 = y.c1",
                        with_clause(scene, false, &format!("q AS ({body})"))
                    ),
                    _ => format!("{} SELECT c1, c2 FROM q", with_clause(scene, false, &format!("q AS ({body})"))),
                };
                Some(Query {
                    sql,
                    ..plain(scene, "", 2, &p, "", c)
                })
            })
        },
    )
}

/// `subquery`: scalar, `IN`, `EXISTS`, correlated and not, and row values.
fn subquery() -> Family {
    family(
        "subquery",
        vec![Axis::new(
            "form",
            &[
                "scalar",
                "in_select",
                "in_table",
                "exists",
                "not_exists",
                "correlated_in",
                "row_value",
            ],
        )],
        shared_allowed,
        |pick| {
            let form = pick.value("form");
            let mut case = read_case(pick, &|scene, c| {
                let (k, a, b) = (&scene.k, &scene.a, &scene.b);
                let (list, p) = match form {
                    "scalar" => (
                        format!("{k} AS c1, (SELECT max(r.w) FROM r WHERE r.a = {a}) AS c2"),
                        format!("{a} IS NOT NULL OR {k} > {}", c.text),
                    ),
                    "in_select" => (
                        format!("{k} AS c1, {a} AS c2"),
                        format!("{a} IN (SELECT r.a FROM r WHERE r.k > {})", c.text),
                    ),
                    "in_table" => (
                        format!("{k} AS c1, {a} AS c2"),
                        format!("{a} IN r1 OR {k} = {}", c.text),
                    ),
                    "exists" => (
                        format!("{k} AS c1, {a} AS c2"),
                        format!(
                            "EXISTS (SELECT 1 FROM r WHERE r.a = {a} AND r.k > {})",
                            c.text
                        ),
                    ),
                    "not_exists" => (
                        format!("{k} AS c1, {a} AS c2"),
                        format!(
                            "NOT EXISTS (SELECT 1 FROM r WHERE r.a = {a}) AND {k} > {}",
                            c.text
                        ),
                    ),
                    "correlated_in" => (
                        format!("{k} AS c1, {a} AS c2"),
                        format!(
                            "{a} IN (SELECT r.a FROM r WHERE r.k >= {k}) OR {k} = {}",
                            c.text
                        ),
                    ),
                    _ => (
                        format!("{k} AS c1, {b} AS c2"),
                        format!(
                            "({a}, {b}) = (SELECT r.a, r.w FROM r WHERE r.k = {k}) OR {k} = {}",
                            c.text
                        ),
                    ),
                };
                Some(plain(scene, &list, 2, &p, "", c))
            })?;
            case.setup.insert(0, Record::ok("CREATE TABLE r1(x)"));
            case.setup.insert(
                1,
                Record::ok("INSERT INTO r1 VALUES (1), (5), ('abc'), (NULL)"),
            );
            if form == "row_value" {
                case.capabilities.push("row_value_in_subquery".to_string());
            }
            Some(with_other(case, "none"))
        },
    )
}

/// The operators and forms of `expression`.
const OPERATORS: &[&str] = &[
    "add",
    "subtract",
    "multiply",
    "divide",
    "remainder",
    "concat",
    "bit_and",
    "bit_or",
    "shift_left",
    "shift_right",
    "bit_not",
    "negate",
    "lt",
    "le",
    "ne",
    "is",
    "is_not",
    "is_distinct",
    "is_not_distinct",
    "and",
    "or",
    "cast_integer",
    "cast_real",
    "cast_text",
    "cast_blob",
    "cast_numeric",
    "collate_nocase",
    "collate_rtrim",
    "like_escape",
    "glob",
    "case_operand",
    "case_searched",
    "between",
    "not_between",
    "isnull",
    "notnull",
    "json_arrow",
    "json_arrow2",
];

/// Writes one expression over the column under test.
fn expression_text(operator: &str, a: &str, b: &str, c: &str) -> String {
    match operator {
        "add" => format!("{a} + {c}"),
        "subtract" => format!("{a} - {c}"),
        "multiply" => format!("{a} * {c}"),
        "divide" => format!("{a} / {c}"),
        "remainder" => format!("{a} % {c}"),
        "concat" => format!("{a} || {b}"),
        "bit_and" => format!("{a} & {c}"),
        "bit_or" => format!("{a} | {c}"),
        "shift_left" => format!("{a} << {c}"),
        "shift_right" => format!("{a} >> {c}"),
        "bit_not" => format!("~{a}"),
        "negate" => format!("-{a}"),
        "lt" => format!("{a} < {c}"),
        "le" => format!("{a} <= {c}"),
        "ne" => format!("{a} <> {c}"),
        "is" => format!("{a} IS {c}"),
        "is_not" => format!("{a} IS NOT {c}"),
        "is_distinct" => format!("{a} IS DISTINCT FROM {c}"),
        "is_not_distinct" => format!("{a} IS NOT DISTINCT FROM {c}"),
        "and" => format!("{a} AND {c}"),
        "or" => format!("{a} OR {b}"),
        "cast_integer" => format!("CAST({a} AS INTEGER)"),
        "cast_real" => format!("CAST({a} AS REAL)"),
        "cast_text" => format!("CAST({a} AS TEXT)"),
        "cast_blob" => format!("CAST({a} AS BLOB)"),
        "cast_numeric" => format!("CAST({a} AS NUMERIC)"),
        "collate_nocase" => format!("{a} = 'ABC' COLLATE NOCASE"),
        "collate_rtrim" => format!("{a} = 'abc  ' COLLATE RTRIM"),
        "like_escape" => format!("{a} LIKE 'a\\%%' ESCAPE '\\'"),
        "glob" => format!("{a} GLOB '[a-c]*'"),
        "case_operand" => format!("CASE {a} WHEN 1 THEN 'one' WHEN {c} THEN 'c' ELSE {b} END"),
        "case_searched" => {
            format!("CASE WHEN {a} > {c} THEN 'big' WHEN {a} IS NULL THEN 'none' END")
        }
        "between" => format!("{a} BETWEEN 1 AND {c}"),
        "not_between" => format!("{a} NOT BETWEEN 1 AND {c}"),
        "isnull" => format!("{a} ISNULL"),
        "notnull" => format!("{a} NOTNULL"),
        "json_arrow" => format!("json_array({a}, {c}) -> 0"),
        _ => format!("json_array({a}, {c}) ->> 1"),
    }
}

/// `expression`: every operator and form, as a result column and as a filter.
fn expression() -> Family {
    family(
        "expression",
        vec![Axis::new("operator", OPERATORS)],
        |pick| {
            shared_allowed(pick)
                && !pick.forbids(
                    "operator",
                    "data",
                    &[("json_arrow", "mixed"), ("json_arrow2", "mixed")],
                )
        },
        |pick| {
            read_case(pick, &|scene, c| {
                let e = expression_text(pick.value("operator"), &scene.a, &scene.b, &c.text);
                let list = format!("{} AS c1, {e} AS c2", scene.k);
                Some(plain(scene, &list, 2, &e, "", c))
            })
        },
    )
}

/// The scalar and aggregate functions `function` crosses with every context.
/// The full list, one case per function, is Layer 1's; these are the ones a
/// defect has been found in or whose answer depends on the context.
const FUNCTIONS: &[&str] = &[
    "abs",
    "length",
    "lower",
    "typeof",
    "quote",
    "hex",
    "round",
    "trim",
    "substr",
    "instr",
    "replace",
    "nullif",
    "ifnull",
    "iif",
    "printf",
    "unicode",
    "json_quote",
    "json_object",
    "min_scalar",
    "sign",
    "count",
    "sum",
    "total",
    "avg",
    "min",
    "max",
    "group_concat",
    "string_agg",
    "json_group_array",
    "json_group_object",
];

/// Whether a function aggregates.
fn aggregates(function: &str) -> bool {
    matches!(
        function,
        "count"
            | "sum"
            | "total"
            | "avg"
            | "min"
            | "max"
            | "group_concat"
            | "string_agg"
            | "json_group_array"
            | "json_group_object"
    )
}

/// Writes a function call over the column under test.
///
/// An aggregate whose answer depends on the order it sees its rows in
/// (`group_concat`, `string_agg`, `json_group_array`, `json_group_object`)
/// always carries an `ORDER BY` that is total, because the order a scan returns
/// rows in is the plan's and two correct engines may differ in it.
fn call(function: &str, scene: &Relation, c: &str, modifier: &str) -> String {
    let (a, b, k) = (&scene.a, &scene.b, &scene.k);
    let distinct = if modifier == "distinct" {
        "DISTINCT "
    } else {
        ""
    };
    let order_sensitive = matches!(
        function,
        "group_concat" | "string_agg" | "json_group_array" | "json_group_object"
    );
    let order = match (order_sensitive, modifier) {
        (true, "distinct") => format!(" ORDER BY {a}"),
        (true, _) => format!(" ORDER BY {a} DESC, {k}"),
        (false, "order_by") => format!(" ORDER BY {a} DESC"),
        _ => String::new(),
    };
    let filter = if modifier == "filter" {
        format!(" FILTER (WHERE {a} > {c})")
    } else {
        String::new()
    };
    let base = match function {
        "abs" => format!("abs({a})"),
        "length" => format!("length({a})"),
        "lower" => format!("lower({a})"),
        "typeof" => format!("typeof({a})"),
        "quote" => format!("quote({a})"),
        "hex" => format!("hex({a})"),
        "round" => format!("round({a}, 1)"),
        "trim" => format!("trim({a})"),
        "substr" => format!("substr({a}, 2)"),
        "instr" => format!("instr({a}, 'b')"),
        "replace" => format!("replace({a}, 'a', 'z')"),
        "nullif" => format!("nullif({a}, {c})"),
        "ifnull" => format!("ifnull({a}, {b})"),
        "iif" => format!("iif({a} > {c}, 'yes', 'no')"),
        "printf" => format!("printf('%s|%d', {a}, {a})"),
        "unicode" => format!("unicode({a})"),
        "json_quote" => format!("json_quote({a})"),
        "json_object" => format!("json_object('a', {a}, 'b', {b})"),
        "min_scalar" => format!("min({a}, {c})"),
        "sign" => format!("sign({a})"),
        "count" => format!("count({distinct}{a}{order}){filter}"),
        "sum" => format!("sum({distinct}{a}){filter}"),
        "total" => format!("total({distinct}{a}){filter}"),
        "avg" => format!("avg({distinct}{a}){filter}"),
        "min" => format!("min({distinct}{a}){filter}"),
        "max" => format!("max({distinct}{a}){filter}"),
        "group_concat" => format!("group_concat({distinct}{a}{order}){filter}"),
        "string_agg" => format!("string_agg({distinct}{a}, ','{order}){filter}"),
        "json_group_array" => format!("json_group_array({distinct}{a}{order}){filter}"),
        _ => format!("json_group_object({b}, {a}{order}){filter}"),
    };
    base
}

/// `function`: functions and aggregates in every context, nested in JSON, over
/// a column with a collation, with `DISTINCT`, `ORDER BY` and `FILTER`.
fn function() -> Family {
    family(
        "function",
        vec![
            Axis::new("function", FUNCTIONS),
            Axis::new("modifier", &["plain", "distinct", "order_by", "filter"]),
            Axis::new("shape", &["bare", "grouped", "nested_json"]),
        ],
        |pick| {
            shared_allowed(pick)
                && !(pick.get("function").is_some_and(|f| !aggregates(f))
                    && pick.get("modifier").is_some_and(|m| m != "plain"))
                && !(pick.get("function").is_some_and(|f| !aggregates(f))
                    && pick.get("shape") == Some("grouped"))
                // DISTINCT takes an aggregate of one argument.
                && !(pick.get("modifier") == Some("distinct")
                    && matches!(pick.get("function"), Some("json_group_object") | Some("string_agg")))
        },
        |pick| {
            read_case(pick, &|scene, c| {
                let f = pick.value("function");
                let expr = call(f, scene, &c.text, pick.value("modifier"));
                let wrapped = if pick.value("shape") == "nested_json" {
                    format!("json_object('v', {expr})")
                } else {
                    expr
                };
                let query = match (aggregates(f), pick.value("shape")) {
                    (true, "grouped") => plain(
                        scene,
                        &format!("{} AS c1, {wrapped} AS c2", scene.b),
                        2,
                        "",
                        &format!("GROUP BY {}", scene.b),
                        c,
                    ),
                    (true, _) => plain(scene, &format!("{wrapped} AS c1"), 1, "", "", c),
                    _ => plain(
                        scene,
                        &format!("{} AS c1, {wrapped} AS c2", scene.k),
                        2,
                        "",
                        "",
                        c,
                    ),
                };
                Some(query)
            })
        },
    )
}
