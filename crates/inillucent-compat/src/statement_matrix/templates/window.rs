//! `window`: every window function, frame unit, bound and exclusion, in every
//! source and placement.
//!
//! Invariant: **every window a case writes orders its rows totally, unless its
//! frame is a `RANGE` with an offset, which SQLite allows only over one
//! ordering term; such a frame is crossed only with the aggregates, whose
//! answer does not depend on the order of peers.** A window over a partial
//! order gives `row_number` and `lag` answers that depend on the plan, and two
//! correct engines may disagree on them.
//!
//! The `beside` axis holds the defects section 1.1 lists: a correlated
//! subquery in the same select list as a window function, a named window, and
//! an aggregate window with `FILTER`. The placement axis holds the others: a
//! window inside a derived table, a CTE and a view.

use crate::statement_matrix::templates::frame::Query;
use crate::statement_matrix::templates::reads::{read_case, shared_allowed, shared_axes};
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The window functions and the aggregates used as windows.
const FUNCTIONS: &[&str] = &[
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lag_default",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "group_concat",
];

/// Whether a function is an aggregate used as a window.
fn aggregate(function: &str) -> bool {
    matches!(
        function,
        "count" | "sum" | "avg" | "min" | "max" | "group_concat"
    )
}

/// The function call, before `FILTER` and `OVER`.
fn call(function: &str, a: &str, c: &str) -> String {
    match function {
        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
            format!("{function}()")
        }
        "ntile" => "ntile(2)".to_string(),
        "lag" => format!("lag({a})"),
        "lag_default" => format!("lag({a}, 1, {c})"),
        "lead" => format!("lead({a}, 2)"),
        "first_value" => format!("first_value({a})"),
        "last_value" => format!("last_value({a})"),
        "nth_value" => format!("nth_value({a}, 2)"),
        "count" => "count(*)".to_string(),
        "group_concat" => format!("group_concat({a}, '.')"),
        other => format!("{other}({a})"),
    }
}

/// The window family.
pub fn family() -> Family {
    let mut axes = vec![
        Axis::new("function", FUNCTIONS),
        Axis::new("unit", &["none", "ROWS", "RANGE", "GROUPS"]),
        Axis::new(
            "start",
            &["UNBOUNDED PRECEDING", "1 PRECEDING", "CURRENT ROW"],
        ),
        Axis::new(
            "end",
            &["CURRENT ROW", "1 FOLLOWING", "UNBOUNDED FOLLOWING"],
        ),
        Axis::new(
            "exclude",
            &["none", "NO OTHERS", "CURRENT ROW", "GROUP", "TIES"],
        ),
        Axis::new("partition", &["none", "by_b"]),
        Axis::new("beside", &["none", "correlated", "named", "filter"]),
    ];
    axes.extend(shared_axes());
    Family {
        name: "window",
        axes,
        allowed: allowed,
        build: build,
    }
}

/// The window family's constraint.
fn allowed(pick: &Pick) -> bool {
    if !shared_allowed(pick) {
        return false;
    }
    let offset = matches!(pick.get("start"), Some("1 PRECEDING"))
        || matches!(pick.get("end"), Some("1 FOLLOWING"));
    let unit = pick.get("unit");
    let function = pick.get("function");
    // A RANGE frame with an offset orders by one term, so only an aggregate's
    // answer is independent of how peers are ordered.
    if unit == Some("RANGE") && offset {
        if function.is_some_and(|f| !aggregate(f) || f == "group_concat") {
            return false;
        }
    }
    if unit == Some("none") && pick.get("exclude").is_some_and(|e| e != "none") {
        return false;
    }
    if pick.get("beside") == Some("filter") && function.is_some_and(|f| !aggregate(f)) {
        return false;
    }
    true
}

/// Builds one window case.
fn build(pick: &Pick) -> Option<crate::statement_matrix::case::Case> {
    let mut case = read_case(pick, &|scene, c| {
        let (k, a, b) = (&scene.k, &scene.a, &scene.b);
        let unit = pick.value("unit");
        let start = pick.value("start");
        let end = pick.value("end");
        let offset = start == "1 PRECEDING" || end == "1 FOLLOWING";
        let order = if unit == "RANGE" && offset {
            format!("ORDER BY {a}")
        } else {
            format!("ORDER BY {a}, {k}")
        };
        let partition = if pick.value("partition") == "by_b" {
            format!("PARTITION BY {b} ")
        } else {
            String::new()
        };
        let frame = if unit == "none" {
            String::new()
        } else {
            let exclude = match pick.value("exclude") {
                "none" => String::new(),
                other => format!(" EXCLUDE {other}"),
            };
            format!(" {unit} BETWEEN {start} AND {end}{exclude}")
        };
        let window = format!("{partition}{order}{frame}");
        let function = pick.value("function");
        let mut expression = call(function, a, &c.text);
        if pick.value("beside") == "filter" {
            expression.push_str(&format!(" FILTER (WHERE {a} > {})", c.text));
        }
        let (over, named) = if pick.value("beside") == "named" {
            ("OVER w".to_string(), format!(" WINDOW w AS ({window})"))
        } else {
            (format!("OVER ({window})"), String::new())
        };
        let mut list = format!("{k} AS c1, {expression} {over} AS c2");
        let mut columns = 2;
        if pick.value("beside") == "correlated" {
            list.push_str(&format!(", (SELECT count(*) FROM r WHERE r.a = {a}) AS c3"));
            columns = 3;
        }
        let tail = format!("WHERE {a} IS NOT NULL OR {k} > {}{named}", c.text);
        Some(Query {
            sql: scene.select(&list, &tail),
            columns,
            binds: c.binds.clone(),
            relation: scene.clone(),
            select_list: format!("{k} AS c1, {a} AS c2"),
            first: k.clone(),
            checkable: true,
            ..Query::default()
        })
    })?;
    case.setup.insert(
        0,
        crate::statement_matrix::case::Record::ok("CREATE TABLE r(k INTEGER, a, w TEXT)"),
    );
    case.setup.insert(
        1,
        crate::statement_matrix::case::Record::ok(
            "INSERT INTO r VALUES (1, 1, 'x'), (2, 1, 'y'), (3, 5, 'z')",
        ),
    );
    Some(case)
}
