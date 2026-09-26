//! `pragma`: each settable pragma read, set and read back, and set and read
//! back after a reopen.
//!
//! Invariant: **a pragma that stores its value in the file must read back the
//! same after the reopen, and one that belongs to the connection must read
//! back its default after it.** The case reopens explicitly between the set
//! and the second read, so both engines' answers after the reopen are graded,
//! not only the rerun the runner does at the end of every writing case.
//! Deliberate default differences (page size, cache size, busy timeout) are
//! rules in `deliberate.toml`.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::frame::record;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The settable pragmas, with two values each to set.
const PRAGMAS: &[(&str, &str, &str)] = &[
    ("user_version", "7", "-3"),
    ("application_id", "12345", "0"),
    ("foreign_keys", "ON", "OFF"),
    ("recursive_triggers", "ON", "OFF"),
    ("case_sensitive_like", "ON", "OFF"),
    ("defer_foreign_keys", "ON", "OFF"),
    ("query_only", "OFF", "OFF"),
    ("automatic_index", "OFF", "ON"),
    ("ignore_check_constraints", "ON", "OFF"),
    ("legacy_alter_table", "ON", "OFF"),
    ("reverse_unordered_selects", "OFF", "OFF"),
    ("secure_delete", "ON", "OFF"),
    ("temp_store", "MEMORY", "FILE"),
    ("synchronous", "OFF", "FULL"),
    ("trusted_schema", "OFF", "ON"),
    ("cell_size_check", "ON", "OFF"),
    ("auto_vacuum", "INCREMENTAL", "NONE"),
    ("journal_mode", "TRUNCATE", "DELETE"),
];

/// The pragma family.
pub fn family() -> Family {
    let names: Vec<&'static str> = PRAGMAS.iter().map(|(name, _, _)| *name).collect();
    Family {
        name: "pragma",
        axes: vec![
            Axis::new("pragma", &names),
            Axis::new(
                "action",
                &["read", "set_read", "set_reopen_read", "set_function_form"],
            ),
            Axis::new("value", &["first", "second"]),
            Axis::new("then", &["nothing", "create_table", "query"]),
        ],
        allowed: |_| true,
        build,
    }
}

/// Builds one pragma case.
fn build(pick: &Pick) -> Option<Case> {
    let name = pick.value("pragma");
    let (_, first, second) = PRAGMAS.iter().find(|(pragma, _, _)| *pragma == name)?;
    let value = if pick.value("value") == "second" {
        second
    } else {
        first
    };
    let read = || record(format!("PRAGMA {name}"), Sort::RowSort, &[]);
    let mut case = Case::new("", "");
    case.records.push(read());
    match pick.value("action") {
        "set_read" => {
            case.records.push(record(
                format!("PRAGMA {name} = {value}"),
                Sort::RowSort,
                &[],
            ));
            case.records.push(read());
        }
        "set_reopen_read" => {
            case.records.push(record(
                format!("PRAGMA {name} = {value}"),
                Sort::RowSort,
                &[],
            ));
            case.records.push(read());
            case.records.push(Record::Reopen);
            case.records.push(read());
        }
        "set_function_form" => {
            case.records.push(record(
                format!("PRAGMA {name}({value})"),
                Sort::RowSort,
                &[],
            ));
            case.records.push(record(
                format!("SELECT * FROM pragma_{name}"),
                Sort::RowSort,
                &[],
            ));
        }
        _ => {}
    }
    match pick.value("then") {
        "create_table" => {
            case.records
                .push(Record::ok("CREATE TABLE after_pragma(a, b)"));
            case.records.push(Record::ok(
                "INSERT INTO after_pragma VALUES (1, 'x'), (2, 'X')",
            ));
            case.records.push(record(
                "SELECT * FROM after_pragma WHERE b LIKE 'x'".to_string(),
                Sort::RowSort,
                &[],
            ));
        }
        "query" => case.records.push(record(
            "SELECT 1, 'a' LIKE 'A'".to_string(),
            Sort::RowSort,
            &[],
        )),
        _ => {}
    }
    Some(case)
}
