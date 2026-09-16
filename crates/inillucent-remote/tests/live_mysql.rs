//! Migrating a **running** MySQL or MariaDB server, end to end.
//!
//! The MySQL half of the acceptance pair. Same discipline as the PostgreSQL
//! one: a real server, a real socket, a real login, and then the published
//! `.rdb` read back with the engine and checked value by value against the
//! literals in `tests/fixtures/mysql.sql` - which the `mysql` client wrote, so
//! the oracle is not the code under test.
//!
//! Skipped when `INILLUCENT_TEST_MYSQL_URL` is unset;
//! `requires = ["mysql"]` in `tests/selection.toml` makes `--strict` count the
//! skip rather than read it as a pass.
//!
//! To set it up:
//!
//! ```text
//! mysql -h 127.0.0.1 -P 3306 -u root -e "CREATE DATABASE inillucent_migrate_test"
//! mysql -h 127.0.0.1 -P 3306 -u root inillucent_migrate_test \
//!       < crates/inillucent-remote/tests/fixtures/mysql.sql
//! set INILLUCENT_TEST_MYSQL_URL=mysql://root@127.0.0.1:3306/inillucent_migrate_test
//! ```
//!
//! Invariant: **this suite says so when it did not run.** It needs a live MySQL
//! server, and a suite that reports success without one is a green that
//! evidences nothing - which is what `inillucent-testrun --strict` exists to
//! make visible.

use std::path::PathBuf;

use inillucent_engine::connect::Database;
use inillucent_remote::migrate::{migrate, Plan};
use inillucent_remote::{ConnectionUrl, RemoteSource};
use inillucent_tree::datum::OwnedDatum;

/// Returns the URL to migrate, or `None` with a reason printed.
///
/// The phrase `; skipping` is what `inillucent-testrun --strict` recognises.
fn url() -> Option<ConnectionUrl> {
    let Ok(text) = std::env::var("INILLUCENT_TEST_MYSQL_URL") else {
        eprintln!(
            "INILLUCENT_TEST_MYSQL_URL is not set, so no MySQL server is available to migrate; \
             skipping. See this file's header for the two commands that set one up."
        );
        return None;
    };
    match ConnectionUrl::parse(&text) {
        Ok(url) => Some(url),
        Err(error) => {
            inillucent_base::testing::skipping(&format!(
                "INILLUCENT_TEST_MYSQL_URL is not a connection URL ({error})"
            ));
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
    let connection = database.session();
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
fn a_live_mysql_database_migrates_verified_and_reads_back() {
    let Some(url) = url() else { return };
    let destination = scratch("mysql");
    let report = migrate(&Plan::new(url, &destination)).expect("the migration runs");

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

    let counted: Vec<(String, u64)> = report
        .tables
        .iter()
        .map(|table| (table.target.clone(), table.rows))
        .collect();
    for (name, rows) in [("note", 3u64), ("reading", 3), ("event", 4), ("unused", 0)] {
        assert!(
            counted.contains(&(name.to_string(), rows)),
            "{name} should have carried {rows} rows; carried {counted:?}"
        );
    }

    let database = Database::open(&destination).expect("the published database opens");

    // 30 significant digits of DECIMAL, carried as its own text.
    assert_eq!(
        text_of(&database, "SELECT price FROM note WHERE id = 1"),
        "12345678901234567890.1234567890"
    );
    // **The value a signed 64-bit integer cannot hold.** `BIGINT UNSIGNED`'s
    // maximum is 18446744073709551615; carried as an integer it would come back
    // as -1, and carried as a double it would come back as 18446744073709552000.
    assert_eq!(
        text_of(&database, "SELECT huge FROM note WHERE id = 1"),
        "18446744073709551615"
    );
    assert_eq!(
        text_of(&database, "SELECT huge FROM note WHERE id = 3"),
        "9223372036854775808"
    );
    // TINYINT(1) is MySQL's boolean and stays 0 and 1.
    assert_eq!(
        text_of(&database, "SELECT published FROM note WHERE id = 1"),
        "1"
    );
    assert_eq!(
        text_of(&database, "SELECT published FROM note WHERE id = 2"),
        "0"
    );
    // Binary columns are bytes.
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
        text_of(&database, "SELECT `odd Name` FROM note WHERE id = 2"),
        ""
    );
    assert_eq!(
        text_of(&database, "SELECT body FROM note WHERE id = 2"),
        "NULL"
    );
    // Text outside ASCII survives.
    assert_eq!(
        text_of(&database, "SELECT body FROM note WHERE id = 3"),
        "unicode: café 🛟"
    );
    // Types this engine has no equivalent for keep the server's rendering.
    assert_eq!(
        text_of(&database, "SELECT mood FROM note WHERE id = 1"),
        "good"
    );
    assert_eq!(
        text_of(&database, "SELECT created FROM note WHERE id = 1"),
        "2026-01-02 03:04:05"
    );

    let named: Vec<String> = report
        .not_carried
        .iter()
        .map(|(kind, name)| format!("{kind} {name}"))
        .collect();
    assert!(
        named.iter().any(|line| line == "view recent"),
        "the view should be reported as not carried: {named:?}"
    );
    assert!(
        named.iter().any(|line| line.contains("note_touch")),
        "the trigger should be reported as not carried: {named:?}"
    );

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

/// The catalog read finds the schema the fixture declares, with the column
/// order, the nullability and the source's own type names.
#[test]
fn the_catalog_is_read_before_a_single_row_moves() {
    let Some(url) = url() else { return };
    let mut source = inillucent_remote::MysqlSource::connect(&url).expect("connects");
    let tables = source.describe().expect("describes");
    let note = tables
        .iter()
        .find(|table| table.name == "note")
        .expect("note is there");
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
            "huge",
            "payload",
            "created",
            "day",
            "document",
            "mood"
        ]
    );
    assert_eq!(note.primary_key, vec!["id".to_string()]);
    assert!(!note.columns[0].nullable, "id is NOT NULL in the source");
    assert!(note.columns[2].nullable, "body is nullable in the source");
    // The composite key comes across in key order, not alphabetically.
    let reading = tables
        .iter()
        .find(|table| table.name == "reading")
        .expect("reading is there");
    assert_eq!(
        reading.primary_key,
        vec!["note".to_string(), "reader".to_string()]
    );
    assert_eq!(source.count(note).expect("counts"), 3);
    source.finish();
}

/// A destination that exists is refused rather than overwritten.
#[test]
fn a_destination_that_exists_is_refused() {
    let Some(url) = url() else { return };
    let destination = scratch("mysql-exists");
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
    let _ = std::fs::remove_file(&destination);
}
