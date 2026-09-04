//! One scenario, asked of both engines, compared in full.
//!
//! Invariant: nothing here decides what "the same" means for a particular
//! feature. It runs a script against a live SQLite process and against
//! rust-db, and compares everything the protocol carries - the rows and their
//! storage classes, the column names, the change counters, the autocommit flag,
//! and, when a statement fails, its primary and extended result codes. A test
//! that wanted a looser comparison would be testing something else.
//!
//! This lived twice over in `dml_differential.rs` and `foreign_keys.rs` before
//! phase 11 needed it six more times. Copying it again would have meant six
//! chances for one copy to compare less than the others and look green.

use std::path::PathBuf;

use crate::oracle::{Driver, Observation, Op, TaggedValue};
use crate::workspace_root;
use rustdb_session::connection::{Connection, OpenOptions, SessionDatabase};
use rustdb_session::statement::Statement;
use rustdb_value::Value;

/// One step of a scenario.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// SQL that is not expected to return rows.
    Exec(&'static str),
    /// SQL whose rows are compared.
    Query(&'static str),
}

/// Returns the pinned SQLite oracle binary, if it has been built.
pub fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RUSTDB_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    // The name is chosen by this platform's executable suffix rather than by
    // trying both, because both exist: the workspace is shared between Windows
    // and WSL, and a Linux run that picked up `sqlite-oracle.exe` would start
    // it through the interop layer and then hand a Windows process a `/mnt/c`
    // path it cannot open.
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Announces a skipped run, in the words the rest of the harness uses.
pub fn announce_skip() {
    eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.{{ps1,sh}}");
}

/// Returns a fresh path for one engine's copy of one scenario.
pub fn scratch(area: &str, name: &str, engine: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output").join(area);
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}-{engine}.db"));
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}-{engine}.db{suffix}")));
    }
    path
}

/// Starts the oracle on a fresh database, or reports why it could not.
pub fn start_oracle(area: &str, name: &str) -> Option<Driver> {
    let program = sqlite_oracle()?;
    let path = scratch(area, name, "sqlite");
    let mut driver = Driver::start("sqlite", &program).ok()?;
    let hello = driver.send(&Op::Hello).ok()?;
    assert!(hello.ok, "the oracle did not answer hello");
    let opened = driver
        .send(&Op::Open(path.display().to_string()))
        .expect("the oracle opens its database");
    assert!(opened.ok, "the oracle could not open {}", path.display());
    Some(driver)
}

/// Opens rust-db on its own copy of a scenario's database.
pub fn start_rustdb(area: &str, name: &str) -> Connection {
    let path = scratch(area, name, "rustdb");
    let database = SessionDatabase::open_with_options(
        &path,
        OpenOptions {
            busy_timeout: std::time::Duration::from_secs(5),
            ..OpenOptions::default()
        },
    )
    .expect("rust-db opens its database");
    database.connect().expect("rust-db connects")
}

/// Renders a rust-db value as the tagged value the protocol carries.
pub fn tagged(value: &Value<'static>) -> TaggedValue {
    match value {
        Value::Null => TaggedValue::Null,
        Value::Integer(integer) => TaggedValue::Integer(*integer),
        Value::Real(real) => TaggedValue::Real(*real),
        Value::Text(text) => TaggedValue::Text(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => TaggedValue::Blob(blob.raw().to_vec()),
    }
}

/// Runs one statement on rust-db and reports it the way the oracle would.
///
/// The shapes have to match exactly, including the parts that are easy to get
/// almost right: a failed statement still reports the connection state after
/// it, and a query that produced no rows still reports its column names.
pub fn observe(connection: &Connection, sql: &str, query: bool) -> Observation {
    let mut observation = Observation::default();
    let outcome = (|| -> Result<(Vec<Vec<TaggedValue>>, Vec<String>), rustdb_base::DbError> {
        let mut rows = Vec::new();
        let mut columns = Vec::new();
        let mut offset = 0usize;
        let bytes = sql.as_bytes();
        while offset < bytes.len() {
            let rest = bytes.get(offset..).unwrap_or(&[]);
            let (mut statement, consumed) = Statement::prepare(connection, rest)?;
            if query {
                columns = statement
                    .columns()
                    .iter()
                    .map(|column| String::from_utf8_lossy(&column.name).into_owned())
                    .collect();
            }
            while statement.step()? {
                if query {
                    rows.push(statement.row().iter().map(tagged).collect());
                }
            }
            statement.finalize()?;
            if consumed == 0 {
                break;
            }
            offset = offset.saturating_add(consumed);
        }
        Ok((rows, columns))
    })();
    let counters = connection.counters();
    match outcome {
        Ok((rows, columns)) => {
            observation.ok = true;
            observation.rows = rows;
            observation.columns = columns;
        }
        Err(failure) => {
            observation.ok = false;
            observation.code = failure.code().value();
            observation.extended = failure.extended().value();
            observation.message = failure.message().to_string();
        }
    }
    observation.changes = counters.changes;
    observation.total_changes = counters.total_changes;
    observation.last_insert_rowid = counters.last_insert_rowid;
    observation.autocommit = connection.autocommit();
    observation
}

/// Compares one observation against the oracle's, failing on any difference.
pub fn compare_observations(
    label: &str,
    sql: &str,
    candidate: &Observation,
    reference: &Observation,
    query: bool,
) {
    compare_with_counters(label, sql, candidate, reference, query, true)
}

/// Compares one observation, saying whether the cumulative counters count.
pub fn compare_with_counters(
    label: &str,
    sql: &str,
    candidate: &Observation,
    reference: &Observation,
    query: bool,
    counters: bool,
) {
    assert_eq!(
        candidate.ok,
        reference.ok,
        "{label} `{sql}`: rust-db {} and SQLite {}\n  rust-db: {}\n  SQLite:  {}",
        if candidate.ok { "succeeded" } else { "failed" },
        if reference.ok { "succeeded" } else { "failed" },
        candidate.message,
        reference.message
    );
    if !reference.ok {
        assert_eq!(
            candidate.code, reference.code,
            "{label} `{sql}`: primary code\n  rust-db: {} ({})\n  SQLite:  {} ({})",
            candidate.code, candidate.message, reference.code, reference.message
        );
        assert_eq!(
            candidate.extended, reference.extended,
            "{label} `{sql}`: extended code\n  rust-db: {} ({})\n  SQLite:  {} ({})",
            candidate.extended, candidate.message, reference.extended, reference.message
        );
        return;
    }
    if query {
        assert_eq!(
            candidate.rows, reference.rows,
            "{label} `{sql}`: rows\n  rust-db: {:?}\n  SQLite:  {:?}",
            candidate.rows, reference.rows
        );
        assert_eq!(
            candidate.columns, reference.columns,
            "{label} `{sql}`: column names",
        );
    }
    // A `CREATE VIRTUAL TABLE` leaves the counters holding whatever the module
    // did to its own shadow tables while it was being made, which is a fact
    // about the module rather than about the caller's rows.
    let module_create = sql
        .trim_start()
        .get(..21)
        .is_some_and(|head| head.eq_ignore_ascii_case("CREATE VIRTUAL TABLE "));
    if !module_create {
        assert_eq!(
            candidate.changes, reference.changes,
            "{label} `{sql}`: changes()",
        );
    }
    if counters && !module_create {
        assert_eq!(
            candidate.total_changes, reference.total_changes,
            "{label} `{sql}`: total_changes()",
        );
        assert_eq!(
            candidate.last_insert_rowid, reference.last_insert_rowid,
            "{label} `{sql}`: last_insert_rowid()",
        );
    }
    assert_eq!(
        candidate.autocommit, reference.autocommit,
        "{label} `{sql}`: autocommit",
    );
}

/// Runs a scenario against both engines and compares every reply.
///
/// Returns how many statements were compared, so a scenario that stopped early
/// cannot look like one that passed. Zero means the oracle was not built.
pub fn compare(area: &str, name: &str, steps: &[Step]) -> usize {
    let Some(mut oracle) = start_oracle(area, name) else {
        announce_skip();
        return 0;
    };
    let connection = start_rustdb(area, name);
    let mut compared = 0usize;
    // The cumulative counters stop being comparable the moment a module is in
    // play, because they then count the module's own statements as well.
    let mut counters = true;
    for (index, step) in steps.iter().enumerate() {
        let (sql, query) = match step {
            Step::Exec(sql) => (*sql, false),
            Step::Query(sql) => (*sql, true),
        };
        if sql
            .trim_start()
            .get(..21)
            .is_some_and(|head| head.eq_ignore_ascii_case("CREATE VIRTUAL TABLE "))
        {
            counters = false;
        }
        let op = if query {
            Op::Query(sql.to_string())
        } else {
            Op::Exec(sql.to_string())
        };
        let reference = oracle.send(&op).expect("the oracle answers");
        let candidate = observe(&connection, sql, query);
        compare_with_counters(
            &format!("step {index}"),
            sql,
            &candidate,
            &reference,
            query,
            counters,
        );
        compared = compared.saturating_add(1);
    }
    let _ = oracle.send(&Op::Bye);
    compared
}

/// Runs one query against both engines and compares the answer.
///
/// The convenience form for a suite whose cases are independent one-line
/// questions - which is what a built-in function's parity looks like.
pub fn compare_queries(area: &str, name: &str, queries: &[&'static str]) -> usize {
    let steps: Vec<Step> = queries.iter().map(|sql| Step::Query(sql)).collect();
    compare(area, name, &steps)
}
