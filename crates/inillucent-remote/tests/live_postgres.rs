//! Migrating a **running** PostgreSQL server, end to end.
//!
//! This is the acceptance test the protocol tests cannot be: it talks to a real
//! server, over a real socket, through a real login, and then reads the
//! published `.rdb` back with the engine and checks the values.
//!
//! **The oracle is not this code.** Every row it asserts was written by
//! `psql` - PostgreSQL's own client - from `tests/fixtures/postgres.sql`, and
//! the expected values are the literals in that file. A wire decoder that is
//! subtly wrong produces a different string here, not a matching one.
//!
//! It needs a server, so it is skipped when `INILLUCENT_TEST_POSTGRES_URL` is
//! unset, and `tests/selection.toml` declares `requires = ["postgres"]` so the
//! runner counts the skip under `--strict` rather than reading it as a pass.
//!
//! To set it up:
//!
//! ```text
//! psql -h 127.0.0.1 -p 5432 -U postgres -c "CREATE DATABASE inillucent_migrate_test"
//! psql -h 127.0.0.1 -p 5432 -U postgres -d inillucent_migrate_test \
//!      -f crates/inillucent-remote/tests/fixtures/postgres.sql
//! set INILLUCENT_TEST_POSTGRES_URL=postgres://postgres@127.0.0.1:5432/inillucent_migrate_test
//! ```
//!
//! Invariant: **this suite says so when it did not run.** It needs a live
//! PostgreSQL server, and a suite that reports success without one is a green
//! that evidences nothing - which is what `inillucent-testrun --strict` exists
//! to make visible.

use std::path::PathBuf;

use inillucent_engine::connect::Database;
use inillucent_remote::migrate::{migrate, Plan};
use inillucent_remote::{ConnectionUrl, RemoteSource};
use inillucent_tree::datum::OwnedDatum;

/// Returns the URL to migrate, or `None` with a reason printed.
///
/// The phrase `; skipping` is what `inillucent-testrun --strict` recognises, so
/// an absent server is counted and named rather than reported as a pass.
fn url() -> Option<ConnectionUrl> {
    let Ok(text) = std::env::var("INILLUCENT_TEST_POSTGRES_URL") else {
        eprintln!(
            "INILLUCENT_TEST_POSTGRES_URL is not set, so no PostgreSQL server is available to \
             migrate; skipping. See this file's header for the two psql commands that set one up."
        );
        return None;
    };
    match ConnectionUrl::parse(&text) {
        Ok(url) => Some(url),
        Err(error) => {
            eprintln!("INILLUCENT_TEST_POSTGRES_URL is not a connection URL ({error}); skipping");
            None
        }
    }
}

/// Returns a scratch path nothing else is using.
///
/// @param name - what to call it
fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inillucent-remote-{name}-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    path
}

/// Returns one column of one row as text.
///
/// @param database - the migrated database
/// @param sql - a query answering one row
fn text_of(database: &Database, sql: &str) -> String {
    let connection = database.connect();
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        Some(OwnedDatum::Int(number)) => number.to_string(),
        Some(OwnedDatum::Real(number)) => format!("{number}"),
        Some(OwnedDatum::Blob(bytes)) => bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        Some(OwnedDatum::Null) => "NULL".to_string(),
        None => "<no row>".to_string(),
    }
}

/// The whole procedure, against a real server, checked value by value.
#[test]
fn a_live_postgres_database_migrates_verified_and_reads_back() {
    let Some(url) = url() else { return };
    let destination = scratch("postgres");
    let plan = Plan::new(url, &destination);
    let report = migrate(&plan).expect("the migration runs");

    for check in &report.checks {
        println!("  {}", check.line());
    }
    println!("  server: {}", report.server);
    println!("  source: {}", report.source);
    assert!(
        report.passed(),
        "checks failed: {:?}",
        report
            .failures()
            .iter()
            .map(|check| check.line())
            .collect::<Vec<String>>()
    );
    assert!(destination.exists(), "the verified database was published");
    // The redaction holds all the way into the report.
    assert!(!report.source.contains("hunter2"));

    // Every table in the fixture, with the row counts its INSERTs wrote.
    let counted: Vec<(String, u64)> = report
        .tables
        .iter()
        .map(|table| (table.target.clone(), table.rows))
        .collect();
    for (name, rows) in [
        ("note", 3u64),
        ("reading", 3),
        ("event", 4),
        ("unused", 0),
        ("sales__order", 2),
        ("sales__note", 1),
    ] {
        assert!(
            counted.contains(&(name.to_string(), rows)),
            "{name} should have carried {rows} rows; carried {counted:?}"
        );
    }

    // **The two `note` tables did not collide.** One is `public.note` and one
    // is `sales.note`, and a migration that dropped the schema would have
    // merged them.
    assert_ne!(
        counted.iter().find(|(name, _)| name == "note"),
        counted.iter().find(|(name, _)| name == "sales__note")
    );

    let database = Database::open(&destination).expect("the published database opens");

    // The value a careless migration rounds: 30 significant digits of numeric,
    // carried as its own text.
    assert_eq!(
        text_of(&database, "SELECT price FROM note WHERE id = 1"),
        "12345678901234567890.1234567890"
    );
    // A boolean is this dialect's 0 and 1.
    assert_eq!(
        text_of(&database, "SELECT published FROM note WHERE id = 1"),
        "1"
    );
    assert_eq!(
        text_of(&database, "SELECT published FROM note WHERE id = 2"),
        "0"
    );
    // Bytes are bytes, not the `\x` text PostgreSQL renders them as.
    assert_eq!(
        text_of(&database, "SELECT payload FROM note WHERE id = 1"),
        "00ff10"
    );
    assert_eq!(
        text_of(&database, "SELECT payload FROM note WHERE id = 2"),
        "deadbeef"
    );
    // An empty string and a NULL are still different things.
    assert_eq!(
        text_of(&database, "SELECT \"odd Name\" FROM note WHERE id = 2"),
        ""
    );
    assert_eq!(
        text_of(&database, "SELECT body FROM note WHERE id = 2"),
        "NULL"
    );
    // Text outside ASCII survives the round trip.
    assert_eq!(
        text_of(&database, "SELECT body FROM note WHERE id = 3"),
        "unicode: café 🛟"
    );
    // A type this engine has no equivalent for keeps the server's rendering.
    assert_eq!(
        text_of(&database, "SELECT tags FROM note WHERE id = 1"),
        "{a,b}"
    );
    assert_eq!(
        text_of(&database, "SELECT created_at FROM note WHERE id = 1"),
        "2026-01-02 03:04:05+00"
    );
    assert_eq!(
        text_of(&database, "SELECT identity FROM note WHERE id = 1"),
        "11111111-2222-3333-4444-555555555555"
    );
    // The composite key came across as a key, not as two ordinary columns.
    let connection = database.connect();
    let key = connection
        .query("SELECT count(*) FROM pragma_index_list('reading')")
        .expect("the pragma runs");
    assert!(
        matches!(key.first().and_then(|row| row.first()), Some(OwnedDatum::Int(count)) if *count > 0),
        "reading should carry its composite primary key"
    );

    // The view and the trigger are reported rather than silently missing.
    let named: Vec<String> = report
        .not_carried
        .iter()
        .map(|(kind, name)| format!("{kind} {name}"))
        .collect();
    assert!(
        named.iter().any(|line| line.contains("public.recent")),
        "the view should be reported as not carried: {named:?}"
    );
    assert!(
        named.iter().any(|line| line.contains("note_touch")),
        "the trigger should be reported as not carried: {named:?}"
    );

    let _ = connection;
    drop(database);
    let _ = std::fs::remove_file(&destination);
    let report_path = destination.with_file_name(format!(
        "{}.migration-report.md",
        destination
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&report_path);
}

/// A second migration to the same path is refused, rather than overwriting a
/// database somebody is already using.
#[test]
fn a_destination_that_exists_is_refused() {
    let Some(url) = url() else { return };
    let destination = scratch("exists");
    std::fs::write(&destination, b"not a database").expect("the file is written");
    let error = migrate(&Plan::new(url, &destination)).expect_err("refuses");
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
        b"not a database".to_vec(),
        "the existing file was not touched"
    );
    let _ = std::fs::remove_file(&destination);
}

/// The catalog read finds the schema the fixture declares, including the
/// column order and the nullability, before anything is copied.
#[test]
fn the_catalog_is_read_before_a_single_row_moves() {
    let Some(url) = url() else { return };
    let mut source = inillucent_remote::PostgresSource::connect(&url).expect("connects");
    let tables = source.describe().expect("describes");
    let note = tables
        .iter()
        .find(|table| table.schema == "public" && table.name == "note")
        .expect("public.note is there");
    let names: Vec<&str> = note
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "id",
            "title",
            "body",
            "odd Name",
            "published",
            "rating",
            "price",
            "tiny",
            "payload",
            "tags",
            "created",
            "created_at",
            "day",
            "identity",
            "document"
        ]
    );
    assert_eq!(note.primary_key, vec!["id".to_string()]);
    assert!(!note.columns[0].nullable, "id is NOT NULL in the source");
    assert!(note.columns[2].nullable, "body is nullable in the source");
    assert_eq!(
        note.columns[6].declared, "numeric(38,10)",
        "the source's own type name is kept for the report"
    );
    // A count is a different query path on the server than the scan.
    assert_eq!(source.count(note).expect("counts"), 3);
    source.finish();
}
