//! The differential oracle: inillucent and pinned SQLite, side by side.
//!
//! Invariant: both sides are child processes speaking the same protocol, and
//! the comparison is of tagged bytes. Nothing here links SQLite into the test
//! binary, and nothing here normalises a value before comparing it.
//!
//! When the pinned reference has not been downloaded these tests report what is
//! missing and return. That is deliberate: the compatibility report is driven
//! by recorded results, so a run without the oracle records nothing and the
//! oracle capabilities stay unevidenced rather than being assumed to pass.

use std::path::PathBuf;

use inillucent_compat::oracle::{compare, Driver, Observation, Op, TaggedValue};
use inillucent_compat::workspace_root;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    // The name is chosen by this platform's executable suffix rather than by
    // trying both, because both exist: the workspace is shared between Windows
    // and WSL, and a Linux run that picked up `sqlite-oracle.exe` would start
    // it through the interop layer and then hand a Windows process a `/mnt/c`
    // path it cannot open. That failed as "unable to open database file",
    // which looks like a permissions problem and is not one.
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns the inillucent oracle binary that cargo built for this test.
fn inillucent_oracle() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_inillucent-oracle"))
}

/// The values every storage class has to survive, including the ones a decimal
/// rendering or a JSON string would quietly change.
fn cases() -> Vec<TaggedValue> {
    vec![
        TaggedValue::Null,
        TaggedValue::Integer(0),
        TaggedValue::Integer(1),
        TaggedValue::Integer(-1),
        TaggedValue::Integer(i64::MIN),
        TaggedValue::Integer(i64::MAX),
        TaggedValue::Integer(9_007_199_254_740_993),
        TaggedValue::Real(0.0),
        TaggedValue::Real(-0.0),
        TaggedValue::Real(0.1),
        TaggedValue::Real(1.0 / 3.0),
        TaggedValue::Real(f64::MIN_POSITIVE),
        TaggedValue::Real(f64::MAX),
        TaggedValue::Real(f64::INFINITY),
        TaggedValue::Real(f64::NEG_INFINITY),
        TaggedValue::Text(Vec::new()),
        TaggedValue::Text(b"hello".to_vec()),
        TaggedValue::Text("héllo ☃ \u{1F600}".as_bytes().to_vec()),
        TaggedValue::Text(vec![0x61, 0x00, 0x62]),
        TaggedValue::Blob(Vec::new()),
        TaggedValue::Blob(vec![0x00]),
        TaggedValue::Blob(vec![0x00, 0xff, 0x7f, 0x80, 0x0a, 0x0d]),
        TaggedValue::Blob((0u8..=255).collect()),
    ]
}

/// Both drivers must return every value exactly as it was sent, and must agree
/// with each other. This is the phase 0 acceptance criterion for the oracle.
#[test]
fn the_oracle_round_trips_every_storage_class_against_sqlite() {
    let Some(program) = sqlite_oracle() else {
        eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.{{ps1,sh}}");
        return;
    };
    let mut sqlite = Driver::start("sqlite", &program).expect("the sqlite oracle starts");
    let mut inillucent =
        Driver::start("inillucent", &inillucent_oracle()).expect("the inillucent oracle starts");

    let hello = sqlite.send(&Op::Hello).expect("the sqlite oracle answers");
    assert!(hello.ok);
    assert!(
        inillucent
            .send(&Op::Hello)
            .expect("the inillucent oracle answers")
            .ok
    );

    sqlite
        .send(&Op::Open(":memory:".to_string()))
        .expect("sqlite opens a memory database");

    for case in cases() {
        let command = Op::Echo(vec![case.clone()]);
        let reference = sqlite.send(&command).expect("sqlite echoes");
        let candidate = inillucent.send(&command).expect("inillucent echoes");
        assert!(
            reference.ok,
            "sqlite refused {case:?}: {}",
            reference.message
        );
        assert!(
            candidate.ok,
            "inillucent refused {case:?}: {}",
            candidate.message
        );
        let returned = reference
            .rows
            .first()
            .and_then(|row| row.first())
            .unwrap_or_else(|| panic!("sqlite returned no value for {case:?}"));
        assert!(
            returned.identical(&case),
            "sqlite changed {case:?} into {returned:?}"
        );
        let differences = compare(&reference, &candidate);
        assert!(
            differences
                .iter()
                .all(|difference| difference.field != "row"),
            "the two drivers disagreed about {case:?}: {differences:#?}"
        );
    }

    // And all of them at once, which exercises multi-parameter binding rather
    // than one value at a time.
    let together = Op::Echo(cases());
    let reference = sqlite.send(&together).expect("sqlite echoes");
    let candidate = inillucent.send(&together).expect("inillucent echoes");
    assert_eq!(
        reference.rows.first().map(Vec::len),
        Some(cases().len()),
        "sqlite returned the wrong number of values"
    );
    let differences = compare(&reference, &candidate);
    assert!(
        differences
            .iter()
            .all(|difference| difference.field != "row"),
        "{differences:#?}"
    );
}

/// An error has to cross the protocol with both result codes and its message
/// intact, or a differential run could not tell two different failures apart.
#[test]
fn the_oracle_reports_the_same_error_as_sqlite() {
    let Some(program) = sqlite_oracle() else {
        eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.{{ps1,sh}}");
        return;
    };
    let mut sqlite = Driver::start("sqlite", &program).expect("the sqlite oracle starts");
    sqlite
        .send(&Op::Open(":memory:".to_string()))
        .expect("sqlite opens a memory database");

    let cases: [(&str, i32, &str); 4] = [
        ("SELECT * FROM no_such_table", 1, "no such table"),
        ("SELECT ", 1, "incomplete input"),
        ("CREATE TABLE", 1, "incomplete input"),
        ("SELECT no_such_function(1)", 1, "no such function"),
    ];
    for (sql, code, needle) in cases {
        let observation = sqlite
            .send(&Op::Query(sql.to_string()))
            .expect("sqlite answers");
        assert!(!observation.ok, "`{sql}` unexpectedly succeeded");
        assert_eq!(observation.code, code, "`{sql}` returned {observation:?}");
        assert!(
            observation.message.contains(needle),
            "`{sql}` said `{}`",
            observation.message
        );
    }

    // inillucent has no SQL front end yet, and its driver must say so rather than
    // inventing an answer. The comparator must see that as a difference.
    let mut inillucent =
        Driver::start("inillucent", &inillucent_oracle()).expect("the inillucent oracle starts");
    let candidate = inillucent
        .send(&Op::Query("SELECT 1".to_string()))
        .expect("inillucent answers");
    assert!(!candidate.ok);
    assert!(
        candidate.message.contains("no SQL front end"),
        "{candidate:?}"
    );
    let reference = Observation {
        ok: true,
        ..Observation::default()
    };
    assert!(!compare(&reference, &candidate).is_empty());
}

/// A successful statement has to carry the connection state a comparison needs:
/// the change counters, the last inserted rowid, and the autocommit flag.
#[test]
fn the_oracle_reports_connection_state() {
    let Some(program) = sqlite_oracle() else {
        eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.{{ps1,sh}}");
        return;
    };
    let mut sqlite = Driver::start("sqlite", &program).expect("the sqlite oracle starts");
    sqlite
        .send(&Op::Open(":memory:".to_string()))
        .expect("sqlite opens a memory database");
    sqlite
        .send(&Op::Exec(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)".to_string(),
        ))
        .expect("the table is created");
    let inserted = sqlite
        .send(&Op::Exec(
            "INSERT INTO t(b) VALUES('x'),('y'),('z')".to_string(),
        ))
        .expect("the rows are inserted");
    assert!(inserted.ok, "{inserted:?}");
    assert_eq!(inserted.changes, 3);
    assert_eq!(inserted.last_insert_rowid, 3);
    assert!(inserted.autocommit);

    let began = sqlite
        .send(&Op::Exec("BEGIN".to_string()))
        .expect("the transaction begins");
    assert!(
        !began.autocommit,
        "autocommit must be off inside a transaction"
    );
    let rows = sqlite
        .send(&Op::Query("SELECT a, b FROM t ORDER BY a".to_string()))
        .expect("the query runs");
    assert_eq!(rows.columns, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0][0], TaggedValue::Integer(1));
    assert_eq!(rows.rows[2][1], TaggedValue::Text(b"z".to_vec()));
}
