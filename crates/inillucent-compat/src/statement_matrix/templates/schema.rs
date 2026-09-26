//! `schema`: objects in `main`, `temp` and an attached database, named bare
//! and qualified, with a `temp` object shadowing a `main` one of the same
//! name, and `sqlite_schema` read afterwards.
//!
//! Invariant: **a bare name resolves to `temp` first, then `main`, then the
//! attached databases in the order they were attached, and each case that
//! makes a name in two schemas asks for it both bare and qualified.** The TEMP
//! trigger and `total_changes` defect the todo service found was a name that
//! resolved to the wrong schema.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::frame::record;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The schema family.
pub fn family() -> Family {
    Family {
        name: "schema",
        axes: vec![
            Axis::new("location", &["main", "temp", "attached"]),
            Axis::new(
                "statement",
                &[
                    "create_select",
                    "insert_select",
                    "create_index",
                    "create_view",
                    "create_trigger",
                    "drop",
                    "alter",
                ],
            ),
            Axis::new("name", &["bare", "qualified"]),
            Axis::new(
                "shadow",
                &["none", "same_name_in_temp", "same_name_in_main"],
            ),
        ],
        allowed: |pick| {
            // A bare name for an attached object only resolves when nothing
            // shadows it; the qualified form is the one that must work.
            !(pick.get("location") == Some("attached")
                && pick.get("name") == Some("bare")
                && pick.get("statement") == Some("create_select"))
        },
        build,
    }
}

/// The schema prefix a location names.
fn prefix(location: &str) -> &'static str {
    match location {
        "temp" => "temp.",
        "attached" => "aux.",
        _ => "main.",
    }
}

/// Builds one schema case.
fn build(pick: &Pick) -> Option<Case> {
    let location = pick.value("location");
    let qualified = pick.value("name") == "qualified";
    let schema = prefix(location);
    let name = |object: &str| {
        if qualified {
            format!("{schema}{object}")
        } else {
            object.to_string()
        }
    };
    let mut case = Case::new("", "");
    case.records
        .push(Record::ok("ATTACH '%SCRATCH%/aux.db' AS aux"));
    let create = match location {
        "temp" => format!("CREATE TEMP TABLE {}(k INTEGER, v TEXT)", name("s")),
        "attached" => "CREATE TABLE aux.s(k INTEGER, v TEXT)".to_string(),
        _ => format!("CREATE TABLE {}(k INTEGER, v TEXT)", name("s")),
    };
    case.records.push(Record::ok(create));
    case.records.push(Record::ok(format!(
        "INSERT INTO {schema}s VALUES (1, '{location}'), (2, '{location}')"
    )));
    match pick.value("shadow") {
        "same_name_in_temp" if location != "temp" => {
            case.records
                .push(Record::ok("CREATE TEMP TABLE s(k INTEGER, v TEXT)"));
            case.records
                .push(Record::ok("INSERT INTO temp.s VALUES (9, 'temp shadow')"));
        }
        "same_name_in_main" if location != "main" => {
            case.records
                .push(Record::ok("CREATE TABLE main.s(k INTEGER, v TEXT)"));
            case.records
                .push(Record::ok("INSERT INTO main.s VALUES (8, 'main shadow')"));
        }
        _ => {}
    }
    let target = name("s");
    let statement = match pick.value("statement") {
        "create_select" => format!(
            "CREATE TABLE {}copy AS SELECT * FROM {target}",
            if qualified { schema } else { "" }
        ),
        "insert_select" => format!("INSERT INTO {target} SELECT k + 10, v FROM {target}"),
        "create_index" => format!("CREATE INDEX {schema}s_k ON s(k)"),
        "create_view" => format!("CREATE TEMP VIEW sv AS SELECT * FROM {target}"),
        "create_trigger" => {
            format!("CREATE TEMP TRIGGER st AFTER INSERT ON {target} BEGIN SELECT 1; END")
        }
        "drop" => format!("DROP TABLE {target}"),
        _ => format!("ALTER TABLE {target} ADD COLUMN extra DEFAULT 'e'"),
    };
    case.records.push(record(statement, Sort::RowSort, &[]));
    for read in [
        "SELECT * FROM s".to_string(),
        format!("SELECT * FROM {schema}s"),
        "SELECT type, name, tbl_name FROM sqlite_schema".to_string(),
        "SELECT type, name, tbl_name FROM sqlite_temp_schema".to_string(),
        "SELECT type, name, tbl_name FROM aux.sqlite_schema".to_string(),
        "SELECT name FROM pragma_database_list".to_string(),
    ] {
        case.records.push(record(read, Sort::RowSort, &[]));
    }
    Some(case)
}
