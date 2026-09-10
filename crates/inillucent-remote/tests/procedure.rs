//! The migration procedure, driven by a **stub source** rather than a socket.
//!
//! `protocol.rs` proves the wire clients and `live_postgres.rs` /
//! `live_mysql.rs` prove the whole thing against real servers. What none of
//! them can reach is the set of failures a real server will not produce on
//! request: a table that reports one count and scans a different number of
//! rows, a scan that dies half way through, a source that holds nothing at all.
//! Those are the paths where "nothing is published" has to hold, and a test
//! that cannot make them happen is not testing them.
//!
//! So the source here is a struct. It can lie.

use std::path::{Path, PathBuf};

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_engine::connect::Database;
use inillucent_remote::migrate::{run, staging_path, Plan};
use inillucent_remote::source::{Kind, SourceColumn, SourceTable};
use inillucent_remote::{ConnectionUrl, RemoteSource};
use inillucent_tree::datum::OwnedDatum;

/// How a stub source should misbehave, if at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// Answer honestly.
    Honest,
    /// Report one more row from `count` than `scan` produced.
    CountsOneMore,
    /// Fail part way through the scan.
    DiesMidScan,
}

/// A source that is a data structure, so a test can make it lie.
struct Stub {
    /// The tables it claims to hold.
    tables: Vec<SourceTable>,
    /// The rows of the first table.
    rows: Vec<Vec<OwnedDatum>>,
    /// How it should misbehave.
    behaviour: Behaviour,
    /// Whether `finish` was called, which the procedure's caller owes it.
    finished: bool,
}

impl Stub {
    /// Returns a source holding one table of three rows.
    ///
    /// @param behaviour - how it should misbehave
    fn new(behaviour: Behaviour) -> Stub {
        Stub {
            tables: vec![SourceTable {
                schema: "public".to_string(),
                name: "note".to_string(),
                target: "note".to_string(),
                columns: vec![
                    SourceColumn {
                        name: "id".to_string(),
                        declared: "bigint".to_string(),
                        kind: Kind::Integer,
                        nullable: false,
                    },
                    SourceColumn {
                        name: "body".to_string(),
                        declared: "text".to_string(),
                        kind: Kind::Text,
                        nullable: true,
                    },
                    SourceColumn {
                        name: "price".to_string(),
                        declared: "numeric".to_string(),
                        kind: Kind::Decimal,
                        nullable: true,
                    },
                ],
                primary_key: vec!["id".to_string()],
            }],
            rows: vec![
                vec![
                    OwnedDatum::Int(1),
                    OwnedDatum::Text(b"first".to_vec()),
                    OwnedDatum::Text(b"12345678901234567890.1234567890".to_vec()),
                ],
                vec![
                    OwnedDatum::Int(2),
                    OwnedDatum::Null,
                    OwnedDatum::Text(b"0.00".to_vec()),
                ],
                vec![
                    OwnedDatum::Int(3),
                    OwnedDatum::Text("café 🛟".as_bytes().to_vec()),
                    OwnedDatum::Null,
                ],
            ],
            behaviour,
            finished: false,
        }
    }

    /// Returns a source that holds no tables at all.
    fn empty() -> Stub {
        Stub {
            tables: Vec::new(),
            rows: Vec::new(),
            behaviour: Behaviour::Honest,
            finished: false,
        }
    }
}

impl RemoteSource for Stub {
    fn describe(&mut self) -> DbResult<Vec<SourceTable>> {
        Ok(self.tables.clone())
    }

    fn count(&mut self, _table: &SourceTable) -> DbResult<u64> {
        let counted = self.rows.len() as u64;
        Ok(match self.behaviour {
            Behaviour::CountsOneMore => counted.saturating_add(1),
            _ => counted,
        })
    }

    fn scan(
        &mut self,
        _table: &SourceTable,
        sink: &mut dyn FnMut(&[OwnedDatum]) -> DbResult<()>,
    ) -> DbResult<u64> {
        let mut sent = 0u64;
        for (at, row) in self.rows.clone().iter().enumerate() {
            if self.behaviour == Behaviour::DiesMidScan && at == 2 {
                return Err(refusal(
                    "the connection went away half way through the table",
                ));
            }
            sink(row)?;
            sent = sent.saturating_add(1);
        }
        Ok(sent)
    }

    fn server(&self) -> String {
        "StubSQL 1.0".to_string()
    }

    fn not_carried(&mut self) -> DbResult<Vec<(String, String)>> {
        Ok(vec![("view".to_string(), "public.recent".to_string())])
    }

    fn finish(&mut self) {
        self.finished = true;
    }
}

/// Returns a scratch path nothing else is using.
///
/// @param name - what to call it
fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inillucent-procedure-{name}-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    path
}

/// Removes everything a migration wrote.
///
/// @param destination - the published path
fn clean(destination: &Path) {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(destination);
    let _ = std::fs::remove_file(staging_path(destination));
    let _ = std::fs::remove_file(destination.with_file_name(format!("{name}.migration-report.md")));
}

/// Returns a plan against a URL that is never dialled.
///
/// @param destination - where to publish
fn plan(destination: &Path) -> Plan {
    let url = ConnectionUrl::parse("postgres://jason:hunter2@example:5432/corpus").expect("parses");
    Plan::new(url, destination)
}

/// The happy path: staged, copied, verified, published, and readable.
#[test]
fn a_verified_migration_publishes_and_reads_back() {
    let destination = scratch("happy");
    let mut source = Stub::new(Behaviour::Honest);
    let report = run(&plan(&destination), &mut source).expect("the migration runs");

    assert!(report.passed(), "{:?}", report.failures());
    assert_eq!(report.rows(), 3);
    assert_eq!(report.server, "StubSQL 1.0");
    assert!(destination.exists(), "the verified database was published");
    assert!(
        !staging_path(&destination).exists(),
        "the staging file was renamed away rather than left beside the destination"
    );

    let database = Database::open(&destination).expect("the published database opens");
    let connection = database.connect();
    let rows = connection
        .query("SELECT id, body, price FROM note ORDER BY id")
        .expect("the query runs");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], OwnedDatum::Int(1));
    assert_eq!(rows[1][1], OwnedDatum::Null, "a null stayed a null");
    assert_eq!(
        rows[2][1],
        OwnedDatum::Text("café 🛟".as_bytes().to_vec()),
        "text outside ASCII survived"
    );
    assert_eq!(
        rows[0][2],
        OwnedDatum::Text(b"12345678901234567890.1234567890".to_vec()),
        "the wide decimal kept every digit"
    );
    let _ = connection;
    drop(database);
    clean(&destination);
}

/// **A source whose count disagrees with its scan publishes nothing.** This is
/// the check that exists because a migration which moved the right number of
/// rows and the wrong bytes passes a count check on its own - here it is the
/// other way round, and it still has to fail.
#[test]
fn a_disagreeing_count_publishes_nothing_and_keeps_the_evidence() {
    let destination = scratch("mismatch");
    let mut source = Stub::new(Behaviour::CountsOneMore);
    let report = run(&plan(&destination), &mut source).expect("the migration runs to a verdict");

    assert!(!report.passed(), "a disagreeing count is not a pass");
    assert!(
        report
            .failures()
            .iter()
            .any(|check| check.name == "source.count.note"),
        "the failing check should name what disagreed: {:?}",
        report
            .checks
            .iter()
            .map(|check| check.line())
            .collect::<Vec<String>>()
    );
    assert!(
        !destination.exists(),
        "nothing that failed a check is ever published"
    );
    assert!(
        staging_path(&destination).exists(),
        "the staging file is left behind, because the thing a person needs after a failed \
         migration is the evidence"
    );
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let written = destination.with_file_name(format!("{name}.migration-report.md"));
    assert!(
        written.exists(),
        "the report is written whether it passed or not"
    );
    let text = std::fs::read_to_string(&written).expect("the report reads");
    assert!(
        !text.contains("hunter2"),
        "the report carries no password: {text}"
    );
    assert!(text.contains("FAIL source.count.note"), "{text}");
    clean(&destination);
}

/// A scan that dies half way is an error naming the table, and it publishes
/// nothing - the rows that did arrive stay in the staging file.
#[test]
fn a_scan_that_dies_publishes_nothing_and_names_the_table() {
    let destination = scratch("died");
    let mut source = Stub::new(Behaviour::DiesMidScan);
    let error = match run(&plan(&destination), &mut source) {
        Ok(report) => panic!("a dead scan should not produce a report: {report:?}"),
        Err(error) => error,
    };
    let said = error.detail().unwrap_or_else(|| error.message());
    assert!(said.contains("public.note"), "{said}");
    assert!(!destination.exists(), "nothing was published");
    assert!(
        staging_path(&destination).exists(),
        "the partial staging file is the evidence and is not cleaned up"
    );
    clean(&destination);
}

/// A source that holds nothing is a pass with one honest check, not an empty
/// report that reads as a success by having no failures in it.
#[test]
fn an_empty_source_says_so_rather_than_passing_vacuously() {
    let destination = scratch("empty");
    let mut source = Stub::empty();
    let report = run(&plan(&destination), &mut source).expect("the migration runs");
    assert!(report.passed());
    assert_eq!(report.rows(), 0);
    assert!(
        report.checks.iter().any(|check| check.name == "tables"),
        "an empty source is stated: {:?}",
        report
            .checks
            .iter()
            .map(|check| check.line())
            .collect::<Vec<String>>()
    );
    assert!(destination.exists());
    clean(&destination);
}

/// A destination that already exists is refused, and the file that was there is
/// not touched.
#[test]
fn an_existing_destination_is_refused_and_left_alone() {
    let destination = scratch("exists");
    std::fs::write(&destination, b"somebody else's database").expect("writes");
    let mut source = Stub::new(Behaviour::Honest);
    let error = match run(&plan(&destination), &mut source) {
        Ok(_) => panic!("an existing destination should be refused"),
        Err(error) => error,
    };
    assert!(
        error
            .detail()
            .unwrap_or_else(|| error.message())
            .contains("never overwrites"),
        "{}",
        error.detail().unwrap_or_else(|| error.message())
    );
    assert_eq!(
        std::fs::read(&destination).expect("still there"),
        b"somebody else's database".to_vec()
    );
    clean(&destination);
}

/// **A leftover staging file is refused rather than resumed.** A file source
/// can be resumed because a file does not change underneath one; a server does,
/// so half of one snapshot joined to half of another is not a database that
/// ever existed.
#[test]
fn a_leftover_staging_file_is_refused_rather_than_resumed() {
    let destination = scratch("leftover");
    std::fs::write(staging_path(&destination), b"half a migration").expect("writes");
    let mut source = Stub::new(Behaviour::Honest);
    let error = match run(&plan(&destination), &mut source) {
        Ok(_) => panic!("a leftover staging file should be refused"),
        Err(error) => error,
    };
    let said = error.detail().unwrap_or_else(|| error.message());
    assert!(said.contains("one pass"), "{said}");
    assert!(!destination.exists());
    clean(&destination);
}

/// The batch size changes how many transactions the copy takes and nothing
/// about what it produces - so the smallest batch and the default must publish
/// the same digest.
#[test]
fn the_batch_size_changes_nothing_about_the_result() {
    let one = scratch("batch-1");
    let mut source = Stub::new(Behaviour::Honest);
    let mut small = plan(&one);
    small.batch = 1;
    let first = run(&small, &mut source).expect("the migration runs");

    let many = scratch("batch-default");
    let mut source = Stub::new(Behaviour::Honest);
    let second = run(&plan(&many), &mut source).expect("the migration runs");

    assert!(first.passed() && second.passed());
    assert_eq!(
        first.tables.first().map(|table| table.digest.clone()),
        second.tables.first().map(|table| table.digest.clone())
    );
    clean(&one);
    clean(&many);
}

/// What the source holds and does not carry across is reported rather than
/// quietly missing.
#[test]
fn objects_that_are_not_carried_are_named_in_the_report() {
    let destination = scratch("not-carried");
    let mut source = Stub::new(Behaviour::Honest);
    let report = run(&plan(&destination), &mut source).expect("the migration runs");
    assert_eq!(
        report.not_carried,
        vec![("view".to_string(), "public.recent".to_string())]
    );
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text =
        std::fs::read_to_string(destination.with_file_name(format!("{name}.migration-report.md")))
            .expect("the report reads");
    assert!(text.contains("Not carried"), "{text}");
    assert!(text.contains("public.recent"), "{text}");
    clean(&destination);
}
