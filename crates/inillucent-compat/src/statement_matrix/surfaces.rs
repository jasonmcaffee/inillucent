//! The ways an application reaches the engine besides one statement at a time
//! on one connection: section 5.5 of `tasks/task-2135-sql-statement-matrix-tdd.md`.
//!
//! Invariant: **a surface is graded against the driver's own `Connection`, not
//! against SQLite.** The statement matrix already grades every Layer 1 case on
//! that path against the oracle; what a surface can get wrong is only where it
//! differs from it. So each Layer 1 case runs record by record on a
//! `Connection`, and then:
//!
//! - through `SharedDatabase`, which runs the same statements on a thread of
//!   its own (a defect with TEMP triggers and `total_changes` was only there);
//! - with every read only query prepared once and run twice, and every query
//!   with bind lines run once per line on one prepared statement;
//! - as one `execute_batch` script, when every statement is expected to
//!   succeed, compared by the schema and the rows it leaves behind (a defect
//!   that split a trigger body at its inner semicolons was only there).
//!
//! Each answer is compared exactly, errors by their status and message. A case
//! that asks for something that changes from run to run (`random()`, the clock,
//! the statement list) is left out, because two runs of it disagree without
//! either surface being wrong.
//!
//! The four capability rows that are about the driver's API rather than a
//! statement have their own cases here, named in [`CASES`] and in
//! `corpora/matrix/capabilities.toml`.

use std::path::{Path, PathBuf};

use inillucent_driver::{Database, OpenOptions, SharedDatabase, Status, Value};

use crate::statement_matrix::case::{split_statements, Case, Expect, Record, Sort};

/// The ids of the cases this module runs through the driver's API.
pub const CASES: &[&str] = &[
    "surface-user-function",
    "surface-user-collation",
    "surface-cancel",
    "surface-readonly-open",
];

/// The surfaces a Layer 1 case runs through, by the name a failure carries.
pub const SURFACES: &[&str] = &["shared", "prepared", "batch"];

/// What one statement answered: its rows, rendered exactly, or its failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The statement succeeded with these rows, one string per row.
    Rows(Vec<String>),
    /// The statement failed with this status and message.
    Failed(String),
}

/// One difference a surface showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    /// The case id, with the surface after an `@`: `ddl-001@shared`.
    pub id: String,
    /// What differed.
    pub detail: String,
}

/// Renders one value so two runs that agree render the same text.
///
/// A real is rendered with `{:?}`, which round trips, because the surfaces are
/// the same engine and must agree to the bit.
///
/// @param value - the value
fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(integer) => integer.to_string(),
        Value::Real(real) => format!("{real:?}"),
        Value::Text(text) => format!("'{text}'"),
        Value::Blob(bytes) => format!("x'{}'", crate::hash::to_hex(bytes)),
    }
}

/// Turns a driver result into an [`Answer`], sorting the rows unless the
/// statement fixed their order.
///
/// @param result - what the driver returned
/// @param sort - how the case compares the rows
fn answer(result: inillucent_driver::Result<inillucent_driver::Rows>, sort: Sort) -> Answer {
    match result {
        Ok(rows) => {
            let mut lines: Vec<String> = rows
                .rows
                .iter()
                .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
                .collect();
            if sort != Sort::NoSort {
                lines.sort();
            }
            Answer::Rows(lines)
        }
        Err(error) => Answer::Failed(format!("{:?}: {}", error.status, error.message)),
    }
}

/// Parses one bind literal into a driver value.
///
/// @param literal - the value as a bind line writes it
fn value_of(literal: &str) -> Value {
    let text = literal.trim();
    if text.eq_ignore_ascii_case("null") {
        return Value::Null;
    }
    if let Ok(integer) = text.parse::<i64>() {
        return Value::Integer(integer);
    }
    if let Ok(real) = text.parse::<f64>() {
        return Value::Real(real);
    }
    if let Some(hex) = text
        .strip_prefix(['x', 'X'])
        .and_then(|rest| rest.strip_prefix('\''))
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return Value::Blob(from_hex(hex));
    }
    let inner = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(text);
    Value::Text(inner.replace("''", "'"))
}

/// Decodes hex digits, ignoring a trailing odd digit.
///
/// @param hex - the digits
fn from_hex(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks(2)
        .filter_map(|pair| std::str::from_utf8(pair).ok())
        .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
        .collect()
}

/// Whether a case can be run twice and give the same answers.
///
/// @param case - the case
pub fn repeatable(case: &Case) -> bool {
    const CHANGING: &[&str] = &[
        "random",
        "'now'",
        "current_",
        "sqlite_stmt",
        "bytecode",
        "%scratch%",
        "database_list",
        "sqlite_offset",
        "last_insert_rowid",
        // The largest rowid, after which the next one is chosen at random.
        "9223372036854775807",
    ];
    case.setup.iter().chain(case.records.iter()).all(|record| {
        record.sql().is_none_or(|sql| {
            let folded = sql.to_ascii_lowercase();
            !CHANGING.iter().any(|word| folded.contains(word))
        })
    })
}

/// Every record of a case, setup first.
///
/// @param case - the case
fn all_records(case: &Case) -> impl Iterator<Item = &Record> {
    case.setup.iter().chain(case.records.iter())
}

/// How a record asks to be compared.
///
/// @param record - the record
fn sort_of(record: &Record) -> Sort {
    match record {
        Record::Query { sort, .. } => *sort,
        _ => Sort::RowSort,
    }
}

/// The value lists a record runs with: one empty list when it has no bind
/// lines.
///
/// @param record - the record
fn runs_of(record: &Record) -> Vec<Vec<Value>> {
    match record {
        Record::Query { binds, .. } if !binds.is_empty() => binds
            .iter()
            .map(|line| line.iter().map(|literal| value_of(literal)).collect())
            .collect(),
        _ => vec![Vec::new()],
    }
}

/// Opens a database for a surface, removing whatever a previous run left.
///
/// @param path - the file
fn fresh(path: &Path) -> Result<Database, String> {
    inillucent_base::testing::remove_database(path);
    open(path)
}

/// Opens a database with a bound on what each statement may spend, so a case
/// that runs away fails instead of growing: the same reason as the runner's
/// `case_budget`.
///
/// @param path - the file
fn open(path: &Path) -> Result<Database, String> {
    let options = OpenOptions {
        limits: inillucent_driver::StatementLimits::served(),
        ..OpenOptions::default()
    };
    Database::open_with(path, options)
        .map_err(|error| format!("open {}: {}", path.display(), error.message))
}

/// Runs one record on one session of the reference path.
///
/// @param database - the database
/// @param session - the session number to continue
/// @param record - the record
fn run_direct(database: &Database, session: u64, record: &Record) -> Vec<Answer> {
    let Some(sql) = record.sql() else {
        return Vec::new();
    };
    let connection = database.session_as(session);
    if split_statements(sql).len() > 1 {
        let done = connection.execute_batch(sql).map(|()| empty_rows());
        return vec![answer(done, Sort::RowSort)];
    }
    runs_of(record)
        .iter()
        .map(|values| answer(connection.query_all(sql, values), sort_of(record)))
        .collect()
}

/// A successful answer with no rows, for a batch.
fn empty_rows() -> inillucent_driver::Rows {
    inillucent_driver::Rows::default()
}

/// Runs one record through a `SharedDatabase`.
///
/// @param database - the shared database
/// @param record - the record
fn run_shared(database: &SharedDatabase, record: &Record) -> Vec<Answer> {
    let Some(sql) = record.sql() else {
        return Vec::new();
    };
    if split_statements(sql).len() > 1 {
        let done = database.execute_batch(sql).map(|()| empty_rows());
        return vec![answer(done, Sort::RowSort)];
    }
    runs_of(record)
        .iter()
        .map(|values| answer(database.query_all(sql, values), sort_of(record)))
        .collect()
}

/// The answers of a whole case on the reference path, one list per record,
/// with the schema and rows it leaves at the end.
///
/// @param case - the case
/// @param path - the database file
fn reference(case: &Case, path: &Path) -> Result<(Vec<Vec<Answer>>, Vec<String>), String> {
    let mut database = fresh(path)?;
    let mut session = database.session().session();
    let mut answers = Vec::new();
    for record in all_records(case) {
        if matches!(record, Record::Reopen) {
            drop(database);
            database = open(path)?;
            session = database.session().session();
        }
        answers.push(run_direct(&database, session, record));
    }
    let state = final_state(|sql| {
        answer(
            database.session_as(session).query_all(sql, &[]),
            Sort::RowSort,
        )
    });
    Ok((answers, state))
}

/// The same case through a `SharedDatabase`.
///
/// @param case - the case
/// @param path - the database file
fn through_shared(case: &Case, path: &Path) -> Result<Vec<Vec<Answer>>, String> {
    inillucent_base::testing::remove_database(path);
    let open = |path: &Path| {
        let options = OpenOptions {
            limits: inillucent_driver::StatementLimits::served(),
            ..OpenOptions::default()
        };
        SharedDatabase::open_with(path, options).map_err(|error| error.message)
    };
    let mut database = open(path)?;
    let mut answers = Vec::new();
    for record in all_records(case) {
        if matches!(record, Record::Reopen) {
            drop(database);
            database = open(path)?;
        }
        answers.push(run_shared(&database, record));
    }
    Ok(answers)
}

/// The schema rows and every ordinary table's rows, as text, for comparing
/// what two ways of running the same statements left behind.
///
/// @param ask - runs one query and answers it
fn final_state(ask: impl Fn(&str) -> Answer) -> Vec<String> {
    let schema = ask("SELECT type, name, tbl_name, sql FROM sqlite_schema");
    let tables = ask(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         AND sql NOT LIKE 'CREATE VIRTUAL%'",
    );
    let mut state = vec![format!("schema {schema:?}")];
    if let Answer::Rows(names) = tables {
        for quoted in names {
            let name = quoted.trim_matches('\'').replace('"', "\"\"");
            let rows = ask(&format!("SELECT * FROM \"{name}\""));
            state.push(format!("{name} {rows:?}"));
        }
    }
    state
}

/// Compares two lists of answers record by record and names the first
/// difference.
///
/// @param case - the case, for its records' text
/// @param expected - the reference answers
/// @param actual - the surface's answers
fn first_difference(
    case: &Case,
    expected: &[Vec<Answer>],
    actual: &[Vec<Answer>],
) -> Option<String> {
    for (index, (record, (want, got))) in all_records(case)
        .zip(expected.iter().zip(actual.iter()))
        .enumerate()
    {
        if want != got {
            return Some(format!(
                "record {index} `{}`: the connection answered {want:?}, this surface answered {got:?}",
                record.sql().unwrap_or("reopen")
            ));
        }
    }
    None
}

/// Runs every read only query of a case twice on one prepared statement, and
/// every query with bind lines once per line on one prepared statement, after
/// the records before it ran on the same session. Returns the first record
/// whose answers differ from the reference.
///
/// @param case - the case
/// @param path - the database file
/// @param expected - the reference answers
fn through_prepared(
    case: &Case,
    path: &Path,
    expected: &[Vec<Answer>],
) -> Result<Option<String>, String> {
    let mut database = fresh(path)?;
    let mut session = database.session().session();
    for (index, record) in all_records(case).enumerate() {
        if matches!(record, Record::Reopen) {
            drop(database);
            database = open(path)?;
            session = database.session().session();
            continue;
        }
        let want = expected.get(index).cloned().unwrap_or_default();
        let got = match record {
            Record::Query { sql, .. } if reusable(sql, record) => {
                let connection = database.session_as(session);
                match prepared_runs(&connection, sql, record) {
                    Ok(runs) => runs,
                    Err(detail) => return Ok(Some(format!("record {index} `{sql}`: {detail}"))),
                }
            }
            _ => run_direct(&database, session, record),
        };
        if got != want {
            return Ok(Some(format!(
                "record {index} `{}`: the connection answered {want:?}, one prepared statement answered {got:?}",
                record.sql().unwrap_or("")
            )));
        }
    }
    Ok(None)
}

/// Whether a query can run on one prepared statement more than once.
///
/// @param sql - the statement
/// @param record - its record
fn reusable(sql: &str, record: &Record) -> bool {
    let bound = matches!(record, Record::Query { binds, .. } if !binds.is_empty());
    split_statements(sql).len() == 1 && (bound || crate::statement_matrix::case::is_read_only(sql))
}

/// Runs one statement on one prepared statement: once per bind line, and,
/// when the statement only reads, once more with the first line's values,
/// which must answer what the first run did.
///
/// @param connection - the session
/// @param sql - the statement
/// @param record - its record
fn prepared_runs(
    connection: &inillucent_driver::Connection<'_>,
    sql: &str,
    record: &Record,
) -> Result<Vec<Answer>, String> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(error) => {
            return Ok(vec![Answer::Failed(format!(
                "{:?}: {}",
                error.status, error.message
            ))])
        }
    };
    let values = runs_of(record);
    let answers: Vec<Answer> = values
        .iter()
        .map(|run| answer(statement.query(run, usize::MAX), sort_of(record)))
        .collect();
    if crate::statement_matrix::case::is_read_only(sql) {
        if let (Some(first_values), Some(first)) = (values.first(), answers.first()) {
            let again = answer(statement.query(first_values, usize::MAX), sort_of(record));
            if &again != first {
                return Err(format!(
                    "the first run of one prepared statement answered {first:?} and the run after it {again:?}"
                ));
            }
        }
    }
    Ok(answers)
}

/// Runs a case's statements as one `execute_batch` script and compares what
/// it leaves with what the record by record run left.
///
/// Only for a case whose every statement is expected to succeed and which
/// never reopens, because a script stops at its first failure.
///
/// @param case - the case
/// @param path - the database file
/// @param expected - the state the reference run left
fn through_batch(case: &Case, path: &Path, expected: &[String]) -> Result<Option<String>, String> {
    // **Every record goes in, the queries too.** A query record can write:
    // `INSERT ... RETURNING` is one, because it returns rows. Leaving the
    // queries out made the script skip those writes, and the difference was
    // reported as `execute_batch` losing them.
    let mut script = String::new();
    for record in all_records(case) {
        if let Record::Statement { sql, .. } | Record::Query { sql, .. } = record {
            script.push_str(sql.trim().trim_end_matches(';'));
            script.push_str(";\n");
        }
    }
    let database = fresh(path)?;
    let connection = database.session();
    if let Err(error) = connection.execute_batch(&script) {
        return Ok(Some(format!(
            "the script failed where every statement succeeded one at a time: {:?}: {}",
            error.status, error.message
        )));
    }
    let state = final_state(|sql| answer(connection.query_all(sql, &[]), Sort::RowSort));
    Ok((state != expected)
        .then(|| format!("the script left {state:?}; one statement at a time left {expected:?}")))
}

/// Whether a case can run as one script: it never reopens, every statement
/// is expected to succeed, every one did succeed on the connection, so a
/// script that stops at a failure is the surface's doing, and no query binds
/// values, since a script has nothing to bind them with.
///
/// @param case - the case
/// @param expected - the reference answers
fn batchable(case: &Case, expected: &[Vec<Answer>]) -> bool {
    let succeeded = expected
        .iter()
        .flatten()
        .all(|answer| matches!(answer, Answer::Rows(_)));
    succeeded
        && all_records(case).all(|record| match record {
            Record::Statement { expect, .. } => *expect == Expect::Ok,
            Record::Query { binds, .. } => binds.is_empty(),
            Record::Reopen => false,
        })
}

/// Runs one Layer 1 case through every surface and returns what differed.
///
/// @param case - the case
/// @param directory - a directory of its own for the case's files
pub fn run_case(case: &Case, directory: &Path) -> Result<Vec<Difference>, String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let file = |name: &str| -> PathBuf { directory.join(name) };
    let (expected, state) = reference(case, &file("reference.rdb"))?;
    let mut differences = Vec::new();
    let mut note = |surface: &str, detail: Option<String>| {
        if let Some(detail) = detail {
            differences.push(Difference {
                id: format!("{}@{surface}", case.id),
                detail,
            });
        }
    };
    let shared = through_shared(case, &file("shared.rdb"))?;
    note("shared", first_difference(case, &expected, &shared));
    note(
        "prepared",
        through_prepared(case, &file("prepared.rdb"), &expected)?,
    );
    if batchable(case, &expected) {
        note("batch", through_batch(case, &file("batch.rdb"), &state)?);
    }
    Ok(differences)
}

/// Runs one of the [`CASES`] and returns why it failed, if it did.
///
/// @param id - the case id
/// @param directory - a directory of its own for the case's files
pub fn run_api_case(id: &str, directory: &Path) -> Result<Option<String>, String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let path = directory.join(format!("{id}.rdb"));
    match id {
        "surface-user-function" => user_function(&path),
        "surface-user-collation" => user_collation(&path),
        "surface-cancel" => cancel(&path),
        "surface-readonly-open" => readonly_open(&path),
        _ => Err(format!("{id} is not a surface case")),
    }
}

/// Checks one query's first value.
///
/// @param connection - the session
/// @param sql - the query
/// @param want - the value it must answer, rendered
fn expect_value(
    connection: &inillucent_driver::Connection<'_>,
    sql: &str,
    want: &str,
) -> Option<String> {
    let got = answer(connection.query_all(sql, &[]), Sort::NoSort);
    (got != Answer::Rows(vec![want.to_string()]))
        .then(|| format!("`{sql}` answered {got:?}, not {want}"))
}

/// A registered scalar function is callable from a statement, and is refused
/// in a schema, which is what the `user_functions` row promises.
///
/// @param path - the database file
fn user_function(path: &Path) -> Result<Option<String>, String> {
    let database = fresh(path)?;
    let connection = database.session();
    connection
        .create_scalar_function("driver_probe", 1, |values| match values.first() {
            Some(Value::Integer(n)) => Ok(Value::Integer(n.saturating_mul(2))),
            _ => Ok(Value::Null),
        })
        .map_err(|error| error.message)?;
    if let Some(problem) = expect_value(&connection, "SELECT driver_probe(2)", "4") {
        return Ok(Some(problem));
    }
    let setup = "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (5)";
    connection
        .execute_batch(setup)
        .map_err(|error| error.message)?;
    if let Some(problem) = expect_value(
        &connection,
        "SELECT sum(driver_probe(a)) FROM t WHERE driver_probe(a) > 2",
        "10",
    ) {
        return Ok(Some(problem));
    }
    // The refusal may come when the table is made or when the CHECK first
    // runs; either keeps code the engine did not write out of the schema.
    let made = connection.execute("CREATE TABLE s(a INTEGER CHECK (driver_probe(a) > 0))", &[]);
    let checked = made.and_then(|_| connection.execute("INSERT INTO s VALUES (1)", &[]));
    Ok(checked
        .is_ok()
        .then(|| "a CHECK constraint calling a registered function ran it".to_string()))
}

/// A registered collation orders a comparison and an index built with it.
///
/// @param path - the database file
fn user_collation(path: &Path) -> Result<Option<String>, String> {
    let database = fresh(path)?;
    let connection = database.session();
    connection
        .create_collation("driver_probe_ci", |left, right| {
            left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
        })
        .map_err(|error| error.message)?;
    if let Some(problem) =
        expect_value(&connection, "SELECT 'B' = 'b' COLLATE driver_probe_ci", "1")
    {
        return Ok(Some(problem));
    }
    let setup = "CREATE TABLE t(a TEXT COLLATE driver_probe_ci); CREATE INDEX ta ON t(a); \
                 INSERT INTO t VALUES ('b'), ('A'), ('C')";
    connection
        .execute_batch(setup)
        .map_err(|error| error.message)?;
    Ok(expect_value(
        &connection,
        "SELECT group_concat(a, '') FROM (SELECT a FROM t ORDER BY a)",
        "'AbC'",
    ))
}

/// A cancel with no statement running cancels nothing: the next statement
/// runs to the end and the connection stays usable.
///
/// **This is the half of the `cancel` row a Rust caller can reach.** The
/// driver's `Database` and `Connection` are neither `Send` nor `Sync`, so safe
/// Rust cannot call `cancel` from another thread while a statement runs, which
/// is the only time it would stop anything. The C API reaches it through a raw
/// handle; the matrix links no C API, so the half that stops a running
/// statement is left to the driver's own suites.
///
/// @param path - the database file
fn cancel(path: &Path) -> Result<Option<String>, String> {
    let database = fresh(path)?;
    let connection = database.session();
    connection.cancel().map_err(|error| error.message)?;
    let counted = "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 50000)                    SELECT count(*) FROM c";
    if let Some(problem) = expect_value(&connection, counted, "50000") {
        return Ok(Some(format!(
            "after a cancel with nothing running, {problem}"
        )));
    }
    Ok(expect_value(&connection, "SELECT 1", "1"))
}

/// A read only open answers queries and refuses a write.
///
/// @param path - the database file
fn readonly_open(path: &Path) -> Result<Option<String>, String> {
    {
        let database = fresh(path)?;
        database
            .session()
            .execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (7)")
            .map_err(|error| error.message)?;
    }
    let options = OpenOptions {
        read_only: true,
        ..OpenOptions::default()
    };
    let database = Database::open_with(path, options).map_err(|error| error.message)?;
    let connection = database.session();
    if let Some(problem) = expect_value(&connection, "SELECT a FROM t", "7") {
        return Ok(Some(problem));
    }
    Ok(match connection.execute("INSERT INTO t VALUES (8)", &[]) {
        Ok(_) => Some("a read only open accepted an INSERT".to_string()),
        Err(error) if error.status == Status::ReadOnly => None,
        Err(error) => Some(format!(
            "the INSERT failed with {:?} rather than ReadOnly",
            error.status
        )),
    })
}

/// Runs one group's share of the Layer 1 cases through the surfaces, and the
/// API cases in group 0 of shard 0, and returns every difference `known.list` does not
/// name and every `known.list` line for a surface that no longer differs.
///
/// @param group - this test function's index
/// @param groups - how many test functions share the cases
/// @param scratch - the directory scratch files go under
pub fn run_group(
    group: usize,
    groups: usize,
    scratch: &Path,
) -> Result<(usize, Vec<String>), String> {
    use crate::statement_matrix::{group as grouping, inventory, known};
    let listed = known::read_known(&known::corpus_root().join("known.list"))?;
    let mut problems = Vec::new();
    let mut ran = Vec::new();
    let mut differing = std::collections::BTreeSet::new();
    for family in inventory::FAMILIES {
        for case in grouping::layer_one(family)? {
            if !grouping::owns(&case.id, group, groups) || !repeatable(&case) {
                continue;
            }
            let directory = scratch.join(format!("{:x}", grouping::id_hash(&case.id)));
            for difference in run_case(&case, &directory)? {
                differing.insert(difference.id.clone());
                if !listed.contains_key(&difference.id) {
                    problems.push(format!(
                        "DIFF {}\n      {}",
                        difference.id, difference.detail
                    ));
                }
            }
            let _ = std::fs::remove_dir_all(&directory);
            ran.push(case.id);
        }
    }
    if group == 0 && grouping::shard().0 == 0 {
        for id in CASES {
            if let Some(problem) = run_api_case(id, &scratch.join(id))? {
                differing.insert((*id).to_string());
                if !listed.contains_key(*id) {
                    problems.push(format!("FAIL {id}\n      {problem}"));
                }
            }
            ran.push((*id).to_string());
        }
    }
    for id in listed.keys() {
        let base = id.split('@').next().unwrap_or(id);
        let mine = id.contains('@') || CASES.contains(&id.as_str());
        if mine && ran.iter().any(|done| done == base) && !differing.contains(id) {
            problems.push(format!(
                "STALE {id}: known.list says this surface differs, and it now agrees. Take the line off."
            ));
        }
    }
    Ok((ran.len(), problems))
}
