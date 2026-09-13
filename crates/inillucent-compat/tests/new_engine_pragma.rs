//! The re-profiled `PRAGMA` set on the new engine.
//!
//! Invariant: **a pragma either means something here or answers the way SQLite
//! answers one it does not recognise, and which of the two it is is written
//! down.** SQLite's own convention for an unknown pragma is a no-op that returns
//! no rows and no error, so a caller for whom the pragma was a hint keeps
//! working and the statement after it runs. The three parts of the set are the
//! ones `newengine::pragma`'s module documentation names, and this is what makes
//! that documentation checkable:
//!
//! - **honoured** - it does what it says;
//! - **answered, fixed** - the engine has exactly one setting and the pragma
//!   reports it, and *refuses* any other, because accepting a setting it cannot
//!   honour would be answering wrongly rather than answering nothing;
//! - **silent** - no rows, no error, for a pragma whose subject does not exist
//!   here.
//!
//! The distinction that matters is between silent and refused, and the tests
//! below are written around it.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::oracle::{Driver, Op};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Builds a small fixture and imports it into the new engine.
///
/// @param tag - what to name the scratch directory after
fn engine(tag: &str) -> Option<ImportedDatabase> {
    let program = oracle_path()?;
    let directory = std::env::temp_dir().join(format!(
        "inillucent-pragma-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let path = directory.join("shop.db");
    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    oracle
        .send(&Op::Open(path.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    for sql in [
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, price REAL DEFAULT 0.0)",
        "CREATE INDEX items_name ON items (name)",
        "INSERT INTO items VALUES (1, 'anvil', 9.5)",
        "INSERT INTO items VALUES (2, 'brick', 1.25)",
    ] {
        let observed = oracle
            .send(&Op::Exec(sql.to_string()))
            .expect("the exec runs");
        assert!(observed.ok, "the fixture did not build: {sql}");
    }
    Some(
        ImportedDatabase::import(path, 8_192)
            .unwrap_or_else(|error| panic!("the fixture did not import: {:?}", error.detail())),
    )
}

/// Runs one pragma and returns its rows as text.
///
/// @param engine - the imported fixture
/// @param sql - the pragma
fn ask(engine: &mut ImportedDatabase, sql: &str) -> Vec<Vec<String>> {
    engine
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: refused: {:?}", error.detail()))
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    OwnedDatum::Null => "null".to_string(),
                    OwnedDatum::Int(number) => number.to_string(),
                    OwnedDatum::Real(number) => format!("{number}"),
                    OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    OwnedDatum::Blob(bytes) => format!("blob:{}", bytes.len()),
                })
                .collect()
        })
        .collect()
}

/// Says the suite could not run, rather than passing quietly.
fn no_oracle() {
    inillucent_compat::differential::skipping(
        "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
    );
}

#[test]
fn the_schema_pragmas_describe_the_new_engine_s_schema() {
    let Some(mut engine) = engine("schema") else {
        return no_oracle();
    };
    let columns = ask(&mut engine, "PRAGMA table_info(items)");
    assert_eq!(columns.len(), 3, "{columns:?}");
    assert_eq!(
        columns.first().map(|row| row.as_slice()),
        Some(
            ["0", "id", "INTEGER", "0", "null", "1"]
                .map(str::to_string)
                .as_slice()
        ),
        "{columns:?}"
    );
    assert_eq!(
        columns
            .get(1)
            .and_then(|row| row.get(3))
            .map(String::as_str),
        Some("1"),
        "name is NOT NULL: {columns:?}"
    );
    assert_eq!(
        columns
            .get(2)
            .and_then(|row| row.get(4))
            .map(String::as_str),
        Some("0.0"),
        "price has a default: {columns:?}"
    );

    let indexes = ask(&mut engine, "PRAGMA index_list(items)");
    assert_eq!(indexes.len(), 1, "{indexes:?}");
    assert_eq!(
        indexes
            .first()
            .and_then(|row| row.get(1))
            .map(String::as_str),
        Some("items_name")
    );
    let key = ask(&mut engine, "PRAGMA index_info(items_name)");
    assert_eq!(
        key.first().map(|row| row.as_slice()),
        Some(["0", "1", "name"].map(str::to_string).as_slice()),
        "{key:?}"
    );
    assert!(
        ask(&mut engine, "PRAGMA table_list")
            .iter()
            .any(|row| row.get(1).map(String::as_str) == Some("items")),
        "table_list does not list the table"
    );
    // A pragma about an object that is not there answers with no rows rather
    // than an error, which is what SQLite does.
    assert!(ask(&mut engine, "PRAGMA table_info(nosuch)").is_empty());
    assert!(ask(&mut engine, "PRAGMA index_info(nosuch)").is_empty());
}

#[test]
fn the_pager_pragmas_describe_the_new_engine_s_file() {
    let Some(mut engine) = engine("pager") else {
        return no_oracle();
    };
    assert_eq!(
        ask(&mut engine, "PRAGMA page_size")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("8192")
    );
    let pages: i64 = ask(&mut engine, "PRAGMA page_count")
        .first()
        .and_then(|row| row.first())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    assert!(pages > 0, "the file has no pages");
    // Negative kibibytes, the way SQLite states a cache size in bytes.
    let cache: i64 = ask(&mut engine, "PRAGMA cache_size")
        .first()
        .and_then(|row| row.first())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    assert!(
        cache < 0,
        "cache_size should be stated in kibibytes: {cache}"
    );
    assert!(
        ask(&mut engine, "PRAGMA database_list")
            .first()
            .and_then(|row| row.get(1))
            .map(String::as_str)
            == Some("main")
    );
}

#[test]
fn the_policy_pragmas_are_honoured_flags() {
    let Some(mut engine) = engine("policy") else {
        return no_oracle();
    };
    assert_eq!(
        ask(&mut engine, "PRAGMA busy_timeout = 250")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("250")
    );
    assert_eq!(
        ask(&mut engine, "PRAGMA busy_timeout")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("250")
    );
    let _ = ask(&mut engine, "PRAGMA foreign_keys = ON");
    assert_eq!(
        ask(&mut engine, "PRAGMA foreign_keys")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("1")
    );
    // `synchronous` is honoured: it is read back as the number SQLite uses.
    let _ = ask(&mut engine, "PRAGMA synchronous = NORMAL");
    assert_eq!(
        ask(&mut engine, "PRAGMA synchronous")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("1")
    );
    let _ = ask(&mut engine, "PRAGMA synchronous = FULL");
    assert_eq!(
        ask(&mut engine, "PRAGMA synchronous")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("2")
    );
}

#[test]
fn a_pragma_with_one_setting_reports_it_and_refuses_any_other() {
    let Some(mut engine) = engine("fixed") else {
        return no_oracle();
    };
    // `delete` is the default because the reference's is, and because the
    // medium gate says it is free: 3.78x weighted with `wal` and 3.70x with
    // `delete`, lower bounds 3.45x and 3.44x over 30 paired rounds.
    // `locking_mode` is the other way round - `normal` is a real switch, and
    // defaulting to it reads 3.03x with a 2.95x lower bound, under the bar.
    for (sql, expected) in [
        ("PRAGMA journal_mode", "delete"),
        ("PRAGMA encoding", "UTF-8"),
        ("PRAGMA locking_mode", "exclusive"),
    ] {
        assert_eq!(
            ask(&mut engine, sql)
                .first()
                .and_then(|row| row.first())
                .map(String::as_str),
            Some(expected),
            "{sql}"
        );
    }
    // **Two of those three no longer report fixed values**, and the settings
    // they report are now the settings they hold rather than the only ones the
    // engine has. A rollback journal exists behind `journal_mode`, so asking for
    // one is answered with the mode that is now in force; the same is true of
    // `locking_mode`, which is what lets a second process onto the file. What
    // makes them still worth asserting here is the *round trip*: the pragma
    // answers with the mode it ended up in, so a caller that asked for `delete`
    // and was given `wal` would know.
    for (sql, expected) in [
        ("PRAGMA journal_mode = WAL", "wal"),
        ("PRAGMA journal_mode = DELETE", "delete"),
        ("PRAGMA journal_mode = MEMORY", "memory"),
        ("PRAGMA journal_mode = WAL", "wal"),
        ("PRAGMA locking_mode = NORMAL", "normal"),
        ("PRAGMA locking_mode = EXCLUSIVE", "exclusive"),
    ] {
        assert_eq!(
            ask(&mut engine, sql)
                .first()
                .and_then(|row| row.first())
                .map(String::as_str),
            Some(expected),
            "{sql}"
        );
    }
    // **Encoding is the one that stayed fixed, and it is still refused rather
    // than accepted quietly.** Every text value in this format is UTF-8, so a
    // caller told it had UTF-16 would go on believing it.
    assert!(
        engine
            .execute_any("PRAGMA encoding = 'UTF-16'", &Params::new())
            .is_err(),
        "PRAGMA encoding = 'UTF-16' should have been refused"
    );
}

#[test]
fn an_unknown_pragma_is_silent_on_the_new_engine() {
    let Some(mut engine) = engine("silent") else {
        return no_oracle();
    };
    // SQLite's own answer to a pragma it has never heard of: no rows, no error.
    //
    // **Only a pragma nobody has heard of stays silent now.** The pragmas on
    // SQLite's own list used to answer this way too - 38 of them - and a caller
    // cannot tell a silent pragma from one that returned no rows. Each of them
    // now answers or refuses; this list is the two that stayed silent, which
    // are the ones a name on nobody's list gets.
    for sql in ["PRAGMA nonesuch", "PRAGMA nonesuch = 4"] {
        assert!(
            ask(&mut engine, sql).is_empty(),
            "{sql} should have answered with no rows"
        );
    }
    // The ones that now report, and the value each reports.
    for (sql, answer) in [
        ("PRAGMA auto_vacuum", "0"),
        ("PRAGMA temp_store", "0"),
        ("PRAGMA query_only", "0"),
        ("PRAGMA mmap_size", "0"),
        ("PRAGMA data_version", "1"),
    ] {
        assert_eq!(
            ask(&mut engine, sql)
                .first()
                .and_then(|row| row.first())
                .map(String::as_str),
            Some(answer),
            "{sql}"
        );
    }
    // And the one that refuses a value this engine cannot be, rather than
    // accepting it and dropping it.
    assert!(
        engine
            .execute_any("PRAGMA temp_store = FILE", &Params::new())
            .is_err(),
        "PRAGMA temp_store = FILE should have refused"
    );
    // **`auto_vacuum` is settable now, and is silently ignored on a database
    // that already has tables** - which is not this engine being lax, it is
    // SQLite's own rule: the mode is a property of the file header and can only
    // be chosen before the first table is created, or changed by a `VACUUM`.
    // Asking for it here is accepted and does nothing, and the pragma keeps
    // reporting what the file actually is.
    assert!(ask(&mut engine, "PRAGMA auto_vacuum = FULL").is_empty());
    assert_eq!(
        ask(&mut engine, "PRAGMA auto_vacuum")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("0")
    );
    // `incremental_vacuum` does nothing here and nothing observable in SQLite
    // either, so it stays a silent no-op rather than becoming a refusal
    // invented by the rule.
    assert!(ask(&mut engine, "PRAGMA incremental_vacuum").is_empty());
    // And the statement after one still runs, which is the property a caller
    // relies on when a pragma is a hint.
    assert_eq!(
        ask(&mut engine, "PRAGMA page_size")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("8192")
    );
}

#[test]
fn the_integrity_check_walks_every_new_engine_tree() {
    let Some(mut engine) = engine("integrity") else {
        return no_oracle();
    };
    assert_eq!(
        ask(&mut engine, "PRAGMA integrity_check")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("ok")
    );
    // After a write, and after a schema change, because both move pages.
    engine
        .execute_any("INSERT INTO items VALUES (3, 'cog', 4.0)", &Params::new())
        .expect("the insert runs");
    engine
        .execute_any("CREATE INDEX items_price ON items (price)", &Params::new())
        .expect("the index is created");
    assert_eq!(
        ask(&mut engine, "PRAGMA quick_check")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("ok")
    );
    // A checkpoint answers three integers, and the first is always zero because
    // there is one writer and it is the caller.
    let checkpoint = ask(&mut engine, "PRAGMA wal_checkpoint");
    assert_eq!(
        checkpoint
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("0"),
        "{checkpoint:?}"
    );
    assert_eq!(
        ask(&mut engine, "PRAGMA integrity_check")
            .first()
            .and_then(|row| row.first())
            .map(String::as_str),
        Some("ok")
    );
}
