//! inillucent and pinned SQLite 3.53.4, asked the same questions.
//!
//! Invariant: SQLite answers as a child process, and every value crosses the
//! boundary as tagged bytes rather than as text. Nothing here links SQLite,
//! and nothing here compares two values after converting one of them - a
//! comparison that normalised first would be a test of the harness.
//!
//! Values reach SQLite by *binding*, never by being written into SQL. There is
//! no literal syntax for the exact bits of a double, and a blob written as
//! `x'..'` has already been through a conversion, so embedding a value in the
//! statement would ask SQLite a different question from the one inillucent was
//! asked. The `bind` op exists for that reason.
//!
//! When the pinned oracle has not been built these tests report what is
//! missing and return. That is deliberate: the compatibility report is driven
//! by recorded results, so a run without the oracle records nothing and the
//! parity rows stay unevidenced rather than being assumed to pass.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use inillucent_compat::corpus;
use inillucent_compat::differential;
use inillucent_compat::fixtures::{malformed_fixtures, valid_fixtures};
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_storage::schema::SchemaKind;
use inillucent_value::affinity::{self, Affinity};
use inillucent_value::cast;
use inillucent_value::collation::{self, Collation};
use inillucent_value::compare::{self, SqlOrdering};
use inillucent_value::record;
use inillucent_value::{TextEncoding, Value};

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

/// Starts the oracle, or reports why it could not and returns `None`.
fn start_oracle() -> Option<Driver> {
    let program = sqlite_oracle()?;
    let mut driver = Driver::start("sqlite", &program).ok()?;
    let hello = driver.send(&Op::Hello).ok()?;
    assert!(hello.ok, "the oracle did not answer hello");
    Some(driver)
}

/// Returns a scratch directory the oracle may open databases in.
///
/// The corpus itself is never opened by the oracle: SQLite may create a
/// journal or a WAL beside a file it opens, and the read-only proof in
/// `storage.rs` asserts that the corpus directory does not change.
fn scratch_dir() -> PathBuf {
    let directory = workspace_root().join("_agent_output/oracle-scratch");
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Copies one fixture into the scratch directory and returns its path.
fn scratch_copy(name: &str) -> PathBuf {
    let target = scratch_dir().join(name);
    let _ = std::fs::copy(corpus::fixture_path(name), &target);
    target
}

/// Renders a inillucent value as the tagged value the protocol carries.
fn tagged(value: &Value<'_>) -> TaggedValue {
    match value {
        Value::Null => TaggedValue::Null,
        Value::Integer(integer) => TaggedValue::Integer(*integer),
        Value::Real(real) => TaggedValue::Real(*real),
        Value::Text(text) => TaggedValue::Text(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => TaggedValue::Blob(blob.raw().to_vec()),
    }
}

/// Turns a tagged value back into a inillucent value.
fn untagged(value: &TaggedValue) -> Value<'static> {
    match value {
        TaggedValue::Null => Value::Null,
        TaggedValue::Integer(integer) => Value::Integer(*integer),
        TaggedValue::Real(real) => Value::Real(*real),
        TaggedValue::Text(bytes) => Value::owned_text(bytes).unwrap_or(Value::Null),
        TaggedValue::Blob(bytes) => Value::owned_blob(bytes).unwrap_or(Value::Null),
    }
}

/// The values every matrix in this file is run over.
///
/// They are chosen for the boundaries rather than for coverage: the two zeroes,
/// the first integer a double cannot hold, the 2^63 edge, text that is a
/// number and text that is nearly one, text that differs only in case or in
/// trailing spaces, and blobs that a text comparison would order differently.
fn matrix_values() -> Vec<TaggedValue> {
    vec![
        TaggedValue::Null,
        TaggedValue::Integer(0),
        TaggedValue::Integer(1),
        TaggedValue::Integer(-1),
        TaggedValue::Integer(127),
        TaggedValue::Integer(-128),
        TaggedValue::Integer(2_147_483_647),
        TaggedValue::Integer(9_007_199_254_740_992),
        TaggedValue::Integer(9_007_199_254_740_993),
        TaggedValue::Integer(i64::MAX),
        TaggedValue::Integer(i64::MIN),
        TaggedValue::Real(0.0),
        TaggedValue::Real(-0.0),
        TaggedValue::Real(0.1),
        TaggedValue::Real(1.0),
        TaggedValue::Real(1.5),
        TaggedValue::Real(-1.5),
        TaggedValue::Real(2.0),
        TaggedValue::Real(9_007_199_254_740_992.0),
        TaggedValue::Real(1e308),
        TaggedValue::Real(f64::MIN_POSITIVE),
        TaggedValue::Real(f64::INFINITY),
        TaggedValue::Real(f64::NEG_INFINITY),
        TaggedValue::Text(Vec::new()),
        TaggedValue::Text(b"0".to_vec()),
        TaggedValue::Text(b"1".to_vec()),
        TaggedValue::Text(b"-1".to_vec()),
        TaggedValue::Text(b"2.0".to_vec()),
        TaggedValue::Text(b"2.5".to_vec()),
        TaggedValue::Text(b"  7  ".to_vec()),
        TaggedValue::Text(b"1e3".to_vec()),
        TaggedValue::Text(b"1e400".to_vec()),
        TaggedValue::Text(b"0x10".to_vec()),
        TaggedValue::Text(b"12abc".to_vec()),
        TaggedValue::Text(b"abc".to_vec()),
        TaggedValue::Text(b"ABC".to_vec()),
        TaggedValue::Text(b"abc ".to_vec()),
        TaggedValue::Text(b"abd".to_vec()),
        TaggedValue::Text(b"9223372036854775807".to_vec()),
        TaggedValue::Text(b"9223372036854775808".to_vec()),
        TaggedValue::Text(b"9007199254740993".to_vec()),
        TaggedValue::Text("h\u{e9}llo".as_bytes().to_vec()),
        TaggedValue::Text("\u{1F600}".as_bytes().to_vec()),
        TaggedValue::Blob(Vec::new()),
        TaggedValue::Blob(vec![0x00]),
        TaggedValue::Blob(b"abc".to_vec()),
        TaggedValue::Blob(b"12".to_vec()),
        TaggedValue::Blob(vec![0xff, 0x00]),
    ]
}

/// A smaller set, for the matrices that are quadratic in their size.
fn pair_values() -> Vec<TaggedValue> {
    matrix_values()
        .into_iter()
        .enumerate()
        .filter(|(index, _)| index % 2 == 0 || *index < 16)
        .map(|(_, value)| value)
        .collect()
}

/// Every row of every fixture must be the row SQLite reads out of that file.
#[test]
fn sqlite_and_inillucent_agree_on_every_row_of_every_fixture() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    let mut compared = 0usize;

    for fixture in valid_fixtures() {
        let copy = scratch_copy(fixture.name);
        let opened = oracle
            .send(&Op::Open(copy.to_string_lossy().into_owned()))
            .expect("the oracle opens the fixture");
        assert!(opened.ok, "{}: {}", fixture.name, opened.message);

        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        let scanned = corpus::scan_database(&mut pager).unwrap();

        for object in &scanned.objects {
            if object.kind != SchemaKind::Table || object.root_page.is_none() {
                continue;
            }
            let rows = scanned.table(&object.name).unwrap_or(&[]);
            let spec = corpus::table_spec(object.sql.as_deref().unwrap_or(""));
            let order = spec.field_order();

            let sql = format!("SELECT * FROM \"{}\"", object.name);
            let answer = oracle.send(&Op::Query(sql.clone())).expect("a query runs");
            assert!(answer.ok, "{}: {}", sql, answer.message);
            assert_eq!(
                answer.rows.len(),
                rows.len(),
                "{} / {}: SQLite returned {} rows and inillucent {}",
                fixture.name,
                object.name,
                answer.rows.len(),
                rows.len()
            );

            for (row_index, (expected, actual)) in answer.rows.iter().zip(rows.iter()).enumerate() {
                for (column, want) in expected.iter().enumerate() {
                    // A record's fields are in the tree's order; a SELECT's
                    // columns are in the table's. For a rowid table they are
                    // the same; for a WITHOUT ROWID table the key comes first.
                    let field = order
                        .iter()
                        .position(|table_column| *table_column == column)
                        .unwrap_or(column);
                    let stored = if spec.rowid_alias() == Some(column) {
                        // The rowid alias is not in the record at all.
                        Value::Integer(actual.rowid.unwrap_or_default())
                    } else {
                        actual.values.get(field).cloned().unwrap_or(Value::Null)
                    };
                    let declared = spec
                        .columns
                        .get(column)
                        .map(|spec| spec.declared.clone())
                        .unwrap_or_default();
                    let seen = corpus::as_a_query_sees_it(stored, &declared);
                    let got = tagged(&seen);
                    assert!(
                        want.identical(&got),
                        "{} / {} row {row_index} column {column}: SQLite says {want:?}, \
                         inillucent says {got:?}",
                        fixture.name,
                        object.name
                    );
                    compared = compared.saturating_add(1);
                }
            }
        }
        oracle.send(&Op::Close).expect("the oracle closes");
    }
    oracle.send(&Op::Bye).ok();
    assert!(compared > 5_000, "only {compared} values were compared");
}

/// An index's entries must be the rows SQLite returns when it uses that index.
#[test]
fn index_order_matches_sqlites_own_order_by() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    let copy = scratch_copy("collations-p1024-utf8.db");
    let opened = oracle
        .send(&Op::Open(copy.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    assert!(opened.ok, "{}", opened.message);

    let (_vfs, mut pager) = corpus::open_fixture("collations-p1024-utf8.db").unwrap();
    let scanned = corpus::scan_database(&mut pager).unwrap();

    for (index_name, order_by) in [
        ("words_binary", "w"),
        ("words_nocase", "w COLLATE NOCASE"),
        ("words_rtrim", "w COLLATE RTRIM"),
    ] {
        let entries = scanned.index(index_name).expect(index_name);
        let sql = format!("SELECT w, rowid FROM words ORDER BY {order_by}, rowid");
        let answer = oracle.send(&Op::Query(sql.clone())).expect("a query runs");
        assert!(answer.ok, "{sql}: {}", answer.message);
        assert_eq!(answer.rows.len(), entries.len(), "{index_name}");
        for (position, (expected, actual)) in answer.rows.iter().zip(entries.iter()).enumerate() {
            let want = expected.first().cloned().unwrap_or(TaggedValue::Null);
            let got = tagged(actual.values.first().unwrap_or(&Value::Null));
            assert!(
                want.identical(&got),
                "{index_name} at {position}: SQLite says {want:?}, the index holds {got:?}"
            );
            // The trailing field of an index entry is the rowid.
            let want_rowid = expected.get(1).cloned().unwrap_or(TaggedValue::Null);
            let got_rowid = tagged(actual.values.last().unwrap_or(&Value::Null));
            assert!(
                want_rowid.identical(&got_rowid),
                "{index_name} at {position}: rowid {want_rowid:?} against {got_rowid:?}"
            );
        }
    }
    oracle.send(&Op::Bye).ok();
}

/// `CAST` must agree with SQLite for every value and every target type.
#[test]
fn cast_matches_sqlite() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    oracle
        .send(&Op::Open(":memory:".to_string()))
        .expect("an in-memory database opens");

    let targets = [
        ("INTEGER", Affinity::Integer),
        ("REAL", Affinity::Real),
        ("TEXT", Affinity::Text),
        ("BLOB", Affinity::Blob),
        ("NUMERIC", Affinity::Numeric),
    ];
    let mut compared = 0usize;
    for value in matrix_values() {
        for (name, affinity_target) in targets {
            let sql = format!("SELECT CAST(?1 AS {name})");
            let answer = oracle
                .send(&Op::Bind {
                    sql: sql.clone(),
                    values: vec![value.clone()],
                })
                .expect("the cast runs");
            assert!(answer.ok, "{sql} on {value:?}: {}", answer.message);
            let want = answer
                .rows
                .first()
                .and_then(|row| row.first())
                .cloned()
                .expect("a cast returns a value");

            let got = cast::cast_value(untagged(&value), affinity_target, TextEncoding::Utf8)
                .expect("inillucent casts");
            let got = tagged(&got);
            assert!(
                want.identical(&got),
                "CAST({value:?} AS {name}): SQLite says {want:?}, inillucent says {got:?}"
            );
            compared = compared.saturating_add(1);
        }
    }
    oracle.send(&Op::Bye).ok();
    assert!(compared > 200, "only {compared} casts were compared");
}

/// Storing a value in a column of each declared type must produce the value
/// SQLite stores, which is what applying an affinity means in practice.
#[test]
fn affinity_on_store_matches_sqlite() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    oracle
        .send(&Op::Open(":memory:".to_string()))
        .expect("an in-memory database opens");

    // One column per declared type, so a single insert exercises every rule.
    let declared: [&str; 8] = [
        "INTEGER",
        "TEXT",
        "REAL",
        "BLOB",
        "NUMERIC",
        "VARCHAR(9)",
        "POINT",
        "",
    ];
    let mut create = String::from("CREATE TABLE t (");
    for (index, declaration) in declared.iter().enumerate() {
        if index > 0 {
            create.push(',');
        }
        create.push_str(&format!("c{index} {declaration}"));
    }
    create.push(')');
    let created = oracle.send(&Op::Exec(create.clone())).expect("create runs");
    assert!(created.ok, "{create}: {}", created.message);

    let mut compared = 0usize;
    for value in matrix_values() {
        let placeholders: Vec<String> = (0..declared.len()).map(|_| "?1".to_string()).collect();
        let sql = format!(
            "INSERT INTO t VALUES ({}) RETURNING *",
            placeholders.join(",")
        );
        let answer = oracle
            .send(&Op::Bind {
                sql: sql.clone(),
                values: vec![value.clone()],
            })
            .expect("the insert runs");
        assert!(answer.ok, "{sql} on {value:?}: {}", answer.message);
        let row = answer.rows.first().cloned().unwrap_or_default();

        for (index, declaration) in declared.iter().enumerate() {
            let want = row.get(index).cloned().unwrap_or(TaggedValue::Null);
            let target = affinity::for_column(declaration.as_bytes());
            let applied = affinity::apply_affinity(untagged(&value), target, TextEncoding::Utf8)
                .expect("inillucent applies the affinity");
            // A REAL column widens an integral value back on the way out.
            let applied = corpus::as_a_query_sees_it(
                applied.into_owned().expect("an owned value"),
                declaration,
            );
            let got = tagged(&applied);
            assert!(
                want.identical(&got),
                "{value:?} into a {declaration:?} column: SQLite says {want:?}, \
                 inillucent says {got:?}"
            );
            compared = compared.saturating_add(1);
        }
    }
    oracle.send(&Op::Bye).ok();
    assert!(compared > 300, "only {compared} stores were compared");
}

/// Comparison must agree with SQLite for every ordered pair of values.
#[test]
fn comparison_matches_sqlite() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    oracle
        .send(&Op::Open(":memory:".to_string()))
        .expect("an in-memory database opens");

    let values = pair_values();
    let mut compared = 0usize;
    for left in &values {
        for right in &values {
            let answer = oracle
                .send(&Op::Bind {
                    sql: "SELECT ?1 < ?2, ?1 = ?2, ?1 > ?2".to_string(),
                    values: vec![left.clone(), right.clone()],
                })
                .expect("the comparison runs");
            assert!(answer.ok, "{left:?} vs {right:?}: {}", answer.message);
            let row = answer.rows.first().cloned().unwrap_or_default();
            let expected = sql_ordering_from(&row);

            let got = compare::compare_sql(&untagged(left), &untagged(right), Collation::Binary);
            assert_eq!(
                expected, got,
                "{left:?} against {right:?}: SQLite says {expected:?}, inillucent says {got:?}"
            );
            compared = compared.saturating_add(1);
        }
    }
    oracle.send(&Op::Bye).ok();
    assert!(compared > 900, "only {compared} pairs were compared");
}

/// Reads the three comparison columns back as one ordering.
fn sql_ordering_from(row: &[TaggedValue]) -> SqlOrdering {
    let truth = |index: usize| -> Option<bool> {
        match row.get(index) {
            Some(TaggedValue::Integer(value)) => Some(*value != 0),
            _ => None,
        }
    };
    match (truth(0), truth(1), truth(2)) {
        (Some(true), _, _) => SqlOrdering::Less,
        (_, Some(true), _) => SqlOrdering::Equal,
        (_, _, Some(true)) => SqlOrdering::Greater,
        (Some(false), Some(false), Some(false)) => SqlOrdering::Equal,
        _ => SqlOrdering::Unknown,
    }
}

/// Every built-in collation must order text exactly as SQLite orders it.
#[test]
fn collations_match_sqlite() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    oracle
        .send(&Op::Open(":memory:".to_string()))
        .expect("an in-memory database opens");

    let texts: Vec<&[u8]> = vec![
        b"",
        b" ",
        b"  ",
        b"a",
        b"A",
        b"a ",
        b"A ",
        b"ab",
        b"AB",
        b"abc",
        b"ABC",
        b"abc ",
        b"abc  ",
        b"abd",
        b"z",
        b"Z",
        "\u{e9}".as_bytes(),
        "\u{c9}".as_bytes(),
        "\u{1F600}".as_bytes(),
        "\u{E000}".as_bytes(),
    ];
    let mut compared = 0usize;
    for (name, collation) in [
        ("BINARY", Collation::Binary),
        ("NOCASE", Collation::NoCase),
        ("RTRIM", Collation::RTrim),
    ] {
        for left in &texts {
            for right in &texts {
                let sql = format!(
                    "SELECT (?1 COLLATE {name}) < ?2, (?1 COLLATE {name}) = ?2, \
                     (?1 COLLATE {name}) > ?2"
                );
                let answer = oracle
                    .send(&Op::Bind {
                        sql,
                        values: vec![
                            TaggedValue::Text(left.to_vec()),
                            TaggedValue::Text(right.to_vec()),
                        ],
                    })
                    .expect("the comparison runs");
                assert!(answer.ok, "{}", answer.message);
                let row = answer.rows.first().cloned().unwrap_or_default();
                let expected = match sql_ordering_from(&row) {
                    SqlOrdering::Less => Ordering::Less,
                    SqlOrdering::Greater => Ordering::Greater,
                    _ => Ordering::Equal,
                };
                let got = collation::compare_text(
                    left,
                    TextEncoding::Utf8,
                    right,
                    TextEncoding::Utf8,
                    collation,
                );
                assert_eq!(
                    expected,
                    got,
                    "{name}: {:?} against {:?}: SQLite says {expected:?}, inillucent says {got:?}",
                    String::from_utf8_lossy(left),
                    String::from_utf8_lossy(right)
                );
                compared = compared.saturating_add(1);
            }
        }
    }
    oracle.send(&Op::Bye).ok();
    assert!(compared > 1_000, "only {compared} comparisons were made");
}

/// Every value must survive a round trip through a real database file, which
/// is the record codec and the B-tree together rather than either alone.
#[test]
fn every_value_survives_a_round_trip_through_a_file() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    let values = matrix_values();
    for encoding_name in ["UTF-8", "UTF-16le", "UTF-16be"] {
        let path = scratch_dir().join(format!("roundtrip-{encoding_name}.db"));
        let _ = std::fs::remove_file(&path);
        let opened = oracle
            .send(&Op::Open(path.to_string_lossy().into_owned()))
            .expect("the database opens");
        assert!(opened.ok, "{}", opened.message);
        oracle
            .send(&Op::Exec(format!("PRAGMA encoding = '{encoding_name}'")))
            .expect("the encoding is set");
        oracle
            .send(&Op::Exec(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v)".to_string(),
            ))
            .expect("the table is created");
        for (index, value) in values.iter().enumerate() {
            let answer = oracle
                .send(&Op::Bind {
                    sql: "INSERT INTO t VALUES (?2, ?1) RETURNING rowid".to_string(),
                    values: vec![value.clone(), TaggedValue::Integer(index as i64 + 1)],
                })
                .expect("the insert runs");
            assert!(answer.ok, "{value:?}: {}", answer.message);
        }
        oracle.send(&Op::Close).expect("the database closes");

        let vfs = inillucent_vfs::OsVfs::new();
        let mut pager = inillucent_storage::pager::Pager::open_read_only(
            &vfs,
            &inillucent_vfs::DbPath::new(&path),
            inillucent_storage::pager::PagerOptions::default(),
        )
        .expect("inillucent opens the round-trip database");
        pager.begin_read().expect("a read starts");
        let scanned = corpus::scan_database(&mut pager).expect("inillucent scans it");
        let rows = scanned.table("t").expect("the table");
        assert_eq!(rows.len(), values.len(), "{encoding_name}");
        for (expected, actual) in values.iter().zip(rows.iter()) {
            let stored = actual.values.get(1).cloned().unwrap_or(Value::Null);
            let got = tagged(&stored);
            assert!(
                expected.identical(&got),
                "{encoding_name}: {expected:?} came back as {got:?}"
            );
        }
        // And the bytes inillucent would write for those values are the bytes
        // SQLite did write.
        let format = pager.header().schema_format;
        for row in rows {
            let re_encoded =
                record::encode_record(&row.values, scanned.encoding, format).expect("re-encode");
            assert_eq!(
                re_encoded, row.payload,
                "{encoding_name}: a record did not re-encode to SQLite's bytes"
            );
        }
    }
    oracle.send(&Op::Bye).ok();
}

/// Every malformed fixture SQLite refuses, inillucent must also refuse.
///
/// The comparison is of *outcome*, not of message: SQLite reports a damaged
/// file as `SQLITE_CORRUPT` or `SQLITE_NOTADB` depending on which field is
/// wrong, and inillucent's job is to refuse the file, not to reproduce the exact
/// sentence.
#[test]
fn sqlite_and_inillucent_both_refuse_the_malformed_corpus() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    for fixture in malformed_fixtures() {
        let copy = scratch_copy(fixture.name);
        let opened = oracle
            .send(&Op::Open(copy.to_string_lossy().into_owned()))
            .expect("the oracle answers");
        let sqlite_refuses = if !opened.ok {
            true
        } else {
            let check = oracle
                .send(&Op::Query("PRAGMA integrity_check".to_string()))
                .expect("the check runs");
            let clean = check.ok
                && check.rows.iter().all(
                    |row| matches!(row.first(), Some(TaggedValue::Text(text)) if text == b"ok"),
                );
            !clean
        };
        oracle.send(&Op::Close).ok();

        let vfs = inillucent_vfs::OsVfs::new();
        let path = inillucent_vfs::DbPath::new(corpus::fixture_path(fixture.name));
        let inillucent_refuses = match inillucent_storage::pager::Pager::open_read_only(
            &vfs,
            &path,
            inillucent_storage::pager::PagerOptions::default(),
        ) {
            Err(_) => true,
            Ok(mut pager) => {
                pager.begin_read().expect("a read starts");
                let scan_failed = corpus::scan_database(&mut pager).is_err();
                pager.clear_sticky_error();
                let check_failed = inillucent_storage::check::check_database(
                    &mut pager,
                    inillucent_storage::check::CheckLevel::Integrity,
                )
                .map(|report| !report.is_ok())
                .unwrap_or(true);
                scan_failed || check_failed
            }
        };

        assert_eq!(
            sqlite_refuses,
            inillucent_refuses,
            "{} ({}): SQLite {} and inillucent {}",
            fixture.name,
            fixture.lie,
            if sqlite_refuses {
                "refused it"
            } else {
                "accepted it"
            },
            if inillucent_refuses {
                "refused it"
            } else {
                "accepted it"
            }
        );
    }
    oracle.send(&Op::Bye).ok();
}

/// Every run-time limit must default to the value SQLite defaults to.
///
/// A limit that is quietly larger accepts a statement SQLite refuses, and one
/// that is quietly smaller refuses a statement SQLite accepts. Neither shows
/// up in a query until the day it matters, so the numbers are compared
/// directly rather than inferred from behaviour.
#[test]
fn every_limit_matches_sqlites_default() {
    let Some(mut oracle) = start_oracle() else {
        differential::announce_skip();
        return;
    };
    let answer = oracle
        .send(&Op::Limits)
        .expect("the oracle reports its limits");
    assert!(answer.ok, "{}", answer.message);
    assert!(!answer.rows.is_empty(), "the oracle reported no limits");

    let ours = inillucent_base::limits::Limits::default();
    let mut compared = 0usize;
    for row in &answer.rows {
        let (Some(TaggedValue::Text(name)), Some(TaggedValue::Integer(value))) =
            (row.first(), row.get(1))
        else {
            panic!("a limit row is not a name and a number: {row:?}");
        };
        let name = String::from_utf8_lossy(name).into_owned();
        let Some(limit) = inillucent_base::limits::LIMIT_ROWS
            .iter()
            .find(|row| row.c_name == name)
        else {
            // A limit inillucent does not model yet is a gap the manifest owns,
            // not a failure here; the manifest's own coverage test catches it.
            continue;
        };
        assert_eq!(
            ours.get(limit.limit),
            *value,
            "{name}: inillucent defaults to {} and SQLite to {value}",
            ours.get(limit.limit)
        );
        compared = compared.saturating_add(1);
    }
    assert!(compared >= 10, "only {compared} limits were compared");

    // And the length limit has to actually bite, on both sides.
    oracle
        .send(&Op::Open(":memory:".to_string()))
        .expect("an in-memory database opens");
    let over = oracle
        .send(&Op::Query(
            "SELECT length(zeroblob(1000000001))".to_string(),
        ))
        .expect("the oracle answers");
    assert!(!over.ok, "SQLite accepted a blob past its length limit");
    assert_eq!(
        over.code,
        inillucent_base::PrimaryCode::TooBig.value(),
        "SQLite reported {} rather than SQLITE_TOOBIG",
        over.code
    );
    assert!(!ours.permits_length(1_000_000_001));
    assert!(ours.permits_length(1_000_000_000));
    oracle.send(&Op::Bye).ok();
}

/// The scratch directory the oracle writes into must be under the gitignored
/// agent-output root, never in the corpus.
#[test]
fn the_oracle_writes_only_under_the_agent_output_root() {
    let scratch = scratch_dir();
    let root = workspace_root().join("_agent_output");
    assert!(
        scratch.starts_with(&root),
        "{} is not under {}",
        scratch.display(),
        root.display()
    );
    assert!(!scratch.starts_with(corpus::corpus_dir()));
    let _ = Path::new(".");
}
