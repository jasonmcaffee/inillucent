//! Importing a SQLite file, and what the driver says when it cannot.
//!
//! Invariant: a schema the pinned SQLite release writes is a schema this driver
//! imports, and a schema it cannot read is reported as the thing that actually
//! went wrong.
//!
//! Both halves were fixed together. `CREATE TABLE pairs (left TEXT, right TEXT)` is an
//! ordinary schema - a diff table, a tree, a stereo channel, a page layout all
//! use those two words - and importing it answered `Corrupt` with *"database
//! disk image is malformed"*. The file was not damaged; SQLite reads it
//! perfectly. Two separate faults produced that one line: the parser treated the
//! join keywords as names nowhere, and the catalog loader reported every parse
//! failure as corruption with the explanation hidden in a field the driver
//! suppresses by default.
//!
//! The fixtures are built by the **pinned shell**, not by this engine, for the
//! same reason `inillucent-compat`'s corpus is: a file we wrote ourselves would
//! only prove we agree with ourselves.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_driver::{Database, Status, Value};

/// Returns the pinned SQLite shell, if this checkout has it.
fn pinned_shell() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join(format!(
        ".sqlite-ref/3.53.4/shell/sqlite3{}",
        std::env::consts::EXE_SUFFIX
    ));
    path.is_file().then_some(path)
}

/// Returns a scratch path nothing else is using.
///
/// @param name - what to call the file
fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inillucent-driver-import-{name}-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    path
}

/// Builds a SQLite database with the pinned shell.
///
/// Returns `None` when the shell is not in this checkout, which is how these
/// tests skip rather than fail on a machine that has not fetched it.
///
/// @param name - what to call the file
/// @param dot_commands - shell settings to apply before the SQL
/// @param sql - the statements to run, semicolon-terminated
fn build_with(name: &str, dot_commands: &[&str], sql: &str) -> Option<PathBuf> {
    let shell = pinned_shell()?;
    let path = scratch(name);
    let _ = std::fs::remove_file(&path);
    let mut command = Command::new(shell);
    command.arg(&path);
    for dot_command in dot_commands {
        command.arg("-cmd").arg(dot_command);
    }
    let output = command.arg(sql).output().expect("the pinned shell runs");
    assert!(
        output.status.success(),
        "{sql}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The shell reports a failed statement on stdout and still exits zero, so
    // the exit status alone would let a fixture that built nothing pass as one
    // that built something.
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(
        !said.contains("Parse error") && !said.contains("Runtime error"),
        "{sql}: {said}"
    );
    assert!(path.is_file(), "the shell wrote no file for `{sql}`");
    Some(path)
}

/// Builds a SQLite database with the pinned shell's own defaults.
///
/// @param name - what to call the file
/// @param sql - the statements to run, semicolon-terminated
fn build(name: &str, sql: &str) -> Option<PathBuf> {
    build_with(name, &[], sql)
}

/// Removes a fixture and the files an import built beside it.
///
/// The imported database is not one file: the engine writes its log as numbered
/// segments beside it, `<name>-wal.0000000001` and so on, and a version of this
/// that removed only the database left one segment per run in the temporary
/// directory for ever. So the segments are removed too, matched by the
/// database's own file name - which carries this process's id and a nanosecond
/// stamp, so the prefix cannot name a file some other test made.
///
/// @param source - the SQLite file the fixture built
/// @param imported - the database the import produced, if it produced one
fn clean_up(source: &Path, imported: Option<&Path>) {
    let _ = std::fs::remove_file(source);
    let Some(imported) = imported else {
        return;
    };
    let _ = std::fs::remove_file(imported);
    let (Some(directory), Some(name)) = (imported.parent(), imported.file_name()) else {
        return;
    };
    let Some(name) = name.to_str() else {
        return;
    };
    let segments = format!("{name}-wal.");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&segments) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The ticket's own reproduction: a table whose columns are named `left` and
/// `right` imports, and its rows read back.
///
/// `LEFT` and `RIGHT` are not reserved in SQLite - its name production accepts
/// the `JOIN_KW` token class directly - so a schema it writes with them in has
/// to load here. This answered `status = Corrupt, message = "database disk
/// image is malformed"` before that was fixed.
#[test]
fn a_table_whose_columns_are_named_left_and_right_imports() {
    let Some(source) = build(
        "reserved",
        "CREATE TABLE pairs (left TEXT, right TEXT); \
         INSERT INTO pairs VALUES ('a','b'); \
         INSERT INTO pairs VALUES ('c','d');",
    ) else {
        eprintln!("the pinned SQLite shell is not in this checkout; skipping");
        return;
    };
    let database = match Database::import_sqlite(&source) {
        Ok(database) => database,
        Err(failure) => {
            clean_up(&source, None);
            panic!("the import must succeed, and said: {failure}");
        }
    };
    let imported = database.path().to_path_buf();
    let connection = database.session();
    let rows = connection
        .query("SELECT left, right FROM pairs ORDER BY left", &[], 16)
        .expect("it queries");
    let names: Vec<&str> = rows
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, vec!["left", "right"]);
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Text("a".to_string()), Value::Text("b".to_string())],
            vec![Value::Text("c".to_string()), Value::Text("d".to_string())],
        ]
    );
    let _ = connection;
    drop(database);
    clean_up(&source, Some(&imported));
}

/// The same words in every other name position the pinned release allows one.
///
/// The rule is per grammar position, so a fix that only widened the column
/// declaration would leave a table *called* `left`, an index called `left` and
/// a qualified `t.left` all still failing. This imports one database that uses
/// all of them.
#[test]
fn the_join_keywords_are_names_in_every_position_sqlite_allows() {
    let Some(source) = build(
        "reserved-positions",
        "CREATE TABLE left (inner TEXT, outer TEXT, cross TEXT, indexed TEXT); \
         INSERT INTO left VALUES ('i','o','c','x'); \
         CREATE INDEX right ON left (inner);",
    ) else {
        eprintln!("the pinned SQLite shell is not in this checkout; skipping");
        return;
    };
    let database = match Database::import_sqlite(&source) {
        Ok(database) => database,
        Err(failure) => {
            clean_up(&source, None);
            panic!("the import must succeed, and said: {failure}");
        }
    };
    let imported = database.path().to_path_buf();
    let connection = database.session();
    let rows = connection
        .query(
            "SELECT left.inner, left.outer, left.cross, left.indexed FROM left",
            &[],
            16,
        )
        .expect("it queries");
    assert_eq!(
        rows.rows,
        vec![vec![
            Value::Text("i".to_string()),
            Value::Text("o".to_string()),
            Value::Text("c".to_string()),
            Value::Text("x".to_string()),
        ]]
    );
    let _ = connection;
    drop(database);
    clean_up(&source, Some(&imported));
}

/// A join keyword is still a join keyword where SQLite says it is one.
///
/// Widening the name rule must not let `LEFT` be eaten as the alias of the table
/// before it. Refusing an outer join is the *right* answer here - the engine
/// says so itself - and what this asserts is that the refusal is about the join
/// rather than about the word `left`.
#[test]
fn a_left_join_is_still_read_as_a_join() {
    let Some(source) = build(
        "reserved-join",
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT); \
         CREATE TABLE u (a INTEGER PRIMARY KEY, c TEXT); \
         INSERT INTO t VALUES (1,'x'); INSERT INTO u VALUES (1,'y');",
    ) else {
        eprintln!("the pinned SQLite shell is not in this checkout; skipping");
        return;
    };
    let database = Database::import_sqlite(&source).expect("it imports");
    let imported = database.path().to_path_buf();
    let connection = database.session();
    // An inner join reads as one and answers.
    let rows = connection
        .query("SELECT t.b, u.c FROM t JOIN u ON t.a = u.a", &[], 16)
        .expect("an inner join runs");
    assert_eq!(rows.total, 1);
    // A left join reads as a *join* - the refusal names the construct, which is
    // only possible if `LEFT` was not read as an alias of `t`.
    match connection.query("SELECT t.b, u.c FROM t LEFT JOIN u ON t.a = u.a", &[], 16) {
        Ok(rows) => assert_eq!(rows.total, 1),
        Err(failure) => {
            assert_eq!(failure.status, Status::Unsupported, "{failure}");
            assert!(
                failure
                    .feature
                    .as_deref()
                    .is_some_and(|feature| feature.contains("join")),
                "{failure:?}"
            );
        }
    }
    let _ = connection;
    drop(database);
    clean_up(&source, Some(&imported));
}

/// Schema SQL this engine cannot parse is reported as what it is, not as a
/// damaged file.
///
/// The fixture is a healthy database whose `sqlite_schema` row was rewritten
/// through `PRAGMA writable_schema`, so every byte of it reads and only the
/// statement is wrong. The old answer - `Corrupt` and *"database disk image is
/// malformed"* - sent a reader to `PRAGMA integrity_check` on a file that
/// passes it.
#[test]
fn a_schema_statement_that_will_not_parse_is_not_reported_as_a_damaged_file() {
    // `.dbconfig defensive off` is what lets `writable_schema` take; the shell
    // refuses to modify `sqlite_schema` otherwise, and refuses it on *stdout*
    // while still exiting zero.
    let Some(source) = build_with(
        "unparseable-schema",
        &[".dbconfig defensive off"],
        "CREATE TABLE good (a); \
         PRAGMA writable_schema = ON; \
         UPDATE sqlite_schema SET sql = 'this is not sql' WHERE name = 'good'; \
         PRAGMA writable_schema = OFF;",
    ) else {
        eprintln!("the pinned SQLite shell is not in this checkout; skipping");
        return;
    };
    let failure = match Database::import_sqlite(&source) {
        Err(failure) => failure,
        Ok(_) => {
            clean_up(&source, None);
            panic!("a schema that will not parse must not import");
        }
    };
    assert_eq!(failure.status, Status::Syntax, "{failure}");
    assert!(failure.message.contains("good"), "{failure:?}");
    assert!(
        failure.message.contains("cannot parse the CREATE TABLE"),
        "{failure:?}"
    );
    assert!(failure.message.contains("syntax error"), "{failure:?}");
    assert!(
        !failure.message.contains("disk image is malformed"),
        "{failure:?}"
    );
    clean_up(&source, None);
}
