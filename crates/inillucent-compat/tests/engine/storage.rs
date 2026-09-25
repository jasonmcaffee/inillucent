//! The read-only storage engine, against databases SQLite created.
//!
//! Invariant: every database here was written by the pinned SQLite 3.53.4
//! binary, and inillucent reads it without ever having been told what is in it.
//! The strongest assertions in this file need no oracle at all, because the
//! *file* is the oracle: a record inillucent decodes and re-encodes must produce
//! the bytes SQLite wrote, a forward scan must be a backward scan reversed,
//! a point seek must find what the scan found, and an integrity check must
//! account for every page in a file another engine laid out.

use std::collections::BTreeSet;

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::PrimaryCode;
use inillucent_compat::corpus::{self, ScannedRow};
use inillucent_compat::fixtures::{malformed_fixtures, valid_fixtures};
use inillucent_storage::check::{self, CheckLevel};
use inillucent_storage::cursor::{BTreeCursor, SeekBias};
use inillucent_storage::pager::{Pager, PagerOptions};
use inillucent_storage::schema::{self, SchemaKind};
use inillucent_value::record::{self, KeyInfo, RecordRef};
use inillucent_value::{Collation, TextEncoding, Value};
use inillucent_vfs::{DbPath, OsVfs};

/// Every valid fixture must open, report a sane header, and scan.
#[test]
fn every_valid_fixture_opens_and_scans() {
    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name)
            .unwrap_or_else(|error| panic!("{} did not open: {error}", fixture.name));
        let header = *pager.header();
        assert!(
            header.page_size.bytes() >= 512,
            "{} has a page size of {}",
            fixture.name,
            header.page_size.bytes()
        );
        assert!(pager.page_count() >= 1, "{} reports no pages", fixture.name);
        let scanned = corpus::scan_database(&mut pager)
            .unwrap_or_else(|error| panic!("{} did not scan: {error}", fixture.name));
        assert!(
            !scanned.objects.is_empty(),
            "{} has an empty schema",
            fixture.name
        );
        // Every table and index the schema names must have been walked.
        let expected_tables = scanned
            .objects
            .iter()
            .filter(|object| object.kind == SchemaKind::Table && object.root_page.is_some())
            .count();
        assert_eq!(
            scanned.tables.len(),
            expected_tables,
            "{} did not scan every table",
            fixture.name
        );
    }
}

/// The header fields a fixture's name promises must be what the file holds.
#[test]
fn each_fixture_has_the_page_size_and_encoding_its_name_declares() {
    for fixture in valid_fixtures() {
        let (_vfs, pager) = corpus::open_fixture(fixture.name).unwrap();
        let declared = inillucent_compat::fixtures::page_size_from_name(fixture.name).unwrap();
        assert_eq!(
            pager.page_size().bytes() as usize,
            declared,
            "{} has the wrong page size",
            fixture.name
        );
        let expected_encoding = if fixture.name.contains("utf16le") {
            TextEncoding::Utf16Le
        } else if fixture.name.contains("utf16be") {
            TextEncoding::Utf16Be
        } else {
            TextEncoding::Utf8
        };
        assert_eq!(
            pager.text_encoding(),
            expected_encoding,
            "{} has the wrong encoding",
            fixture.name
        );
    }
    // The reserved-bytes fixture is the one whose usable size is not its page
    // size, which every payload calculation depends on.
    let (_vfs, pager) = corpus::open_fixture("reserved-p4096-utf8.db").unwrap();
    assert_eq!(pager.header().reserved_bytes, 32);
    assert_eq!(pager.usable_size().unwrap(), 4064);
}

/// The record codec must reproduce SQLite's bytes exactly.
///
/// This is the strongest statement phase 2 can make and it needs no oracle
/// process: the payload on the page *is* what SQLite encoded, so decoding it
/// and encoding it again has to land on the same bytes. A different serial
/// type, a different header width, or a different text encoding all show up
/// here as a mismatch.
#[test]
fn every_record_re_encodes_to_the_bytes_sqlite_wrote() {
    let mut checked = 0usize;
    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        let encoding = pager.text_encoding();
        let format = pager.header().schema_format;
        let scanned = corpus::scan_database(&mut pager).unwrap();
        for (name, rows) in scanned.tables.iter().chain(scanned.indexes.iter()) {
            for row in rows {
                let re_encoded = record::encode_record(&row.values, encoding, format)
                    .unwrap_or_else(|error| {
                        panic!("{} / {name}: re-encoding failed: {error}", fixture.name)
                    });
                assert_eq!(
                    re_encoded, row.payload,
                    "{} / {name}: a record did not re-encode to SQLite's bytes ({:?})",
                    fixture.name, row.values
                );
                checked = checked.saturating_add(1);
            }
        }
    }
    assert!(checked > 2_000, "only {checked} records were checked");
}

/// A backward scan must be a forward scan reversed, on every tree.
#[test]
fn a_backward_scan_is_a_forward_scan_reversed() {
    let limits = Limits::default();
    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        let encoding = pager.text_encoding();
        let objects = schema::load_schema(&mut pager).unwrap();
        for object in &objects {
            let Some(root) = object.root_page else {
                continue;
            };
            let is_table = object.kind == SchemaKind::Table;
            let kind = if is_table {
                corpus::RootKind::Table
            } else {
                corpus::RootKind::Index
            };
            let forward = corpus::scan_tree(&mut pager, root, kind, &limits, encoding).unwrap();
            let backward =
                corpus::scan_tree_backwards(&mut pager, root, is_table, &limits, encoding).unwrap();
            assert_eq!(
                forward.len(),
                backward.len(),
                "{} / {}: {} forwards but {} backwards",
                fixture.name,
                object.name,
                forward.len(),
                backward.len()
            );
            let reversed: Vec<&ScannedRow> = backward.iter().rev().collect();
            for (index, (ahead, behind)) in forward.iter().zip(reversed.iter()).enumerate() {
                assert_eq!(
                    ahead.payload, behind.payload,
                    "{} / {}: entry {index} differs between directions",
                    fixture.name, object.name
                );
                assert_eq!(ahead.rowid, behind.rowid);
            }
        }
    }
}

/// A table scan must come back in strictly increasing rowid order, and an
/// index scan in non-decreasing key order.
#[test]
fn scans_come_back_in_key_order() {
    let limits = Limits::default();
    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        let encoding = pager.text_encoding();
        let scanned = corpus::scan_database(&mut pager).unwrap();
        for (name, rows) in &scanned.tables {
            let mut previous: Option<i64> = None;
            for row in rows {
                if let Some(rowid) = row.rowid {
                    if let Some(before) = previous {
                        assert!(
                            rowid > before,
                            "{} / {name}: rowid {rowid} came after {before}",
                            fixture.name
                        );
                    }
                    previous = Some(rowid);
                }
            }
        }
        for (name, rows) in &scanned.indexes {
            // The order an index is in comes from its declaration - the
            // collation and the direction - which is SQL text that storage
            // does not read. The suite parses it here and asserts against the
            // order the declaration promises rather than against a default
            // that would call every DESC index corrupt.
            let sql = scanned
                .objects
                .iter()
                .find(|object| &object.name == name)
                .and_then(|object| object.sql.clone())
                .unwrap_or_default();
            let key = corpus::index_key_info(&sql);
            let mut previous: Option<Vec<u8>> = None;
            for row in rows {
                if let Some(before) = previous.as_ref() {
                    let left = RecordRef::parse_with_limits(before, encoding, &limits).unwrap();
                    let right =
                        RecordRef::parse_with_limits(&row.payload, encoding, &limits).unwrap();
                    let ordering = record::compare_records(&left, &right, &key).unwrap();
                    assert_eq!(
                        ordering,
                        std::cmp::Ordering::Less,
                        "{} / {name}: index entries are out of order at {:?} (declared {sql})",
                        fixture.name,
                        row.values
                    );
                }
                previous = Some(row.payload.clone());
            }
        }
    }
}

/// A point seek must find every rowid a scan found, and no rowid it did not.
#[test]
fn a_point_seek_agrees_with_a_scan() {
    let limits = Limits::default();
    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        let objects = schema::load_schema(&mut pager).unwrap();
        for object in &objects {
            let Some(root) = object.root_page else {
                continue;
            };
            if object.kind != SchemaKind::Table || !corpus::root_is_table(&mut pager, root).unwrap()
            {
                continue;
            }
            let encoding = pager.text_encoding();
            let rows =
                corpus::scan_tree(&mut pager, root, corpus::RootKind::Table, &limits, encoding)
                    .unwrap();
            let present: BTreeSet<i64> = rows.iter().filter_map(|row| row.rowid).collect();
            for row in &rows {
                let rowid = row.rowid.unwrap();
                let found = corpus::seek_rowid(&mut pager, root, rowid, &limits)
                    .unwrap()
                    .unwrap_or_else(|| {
                        panic!(
                            "{} / {}: rowid {rowid} was scanned but not found",
                            fixture.name, object.name
                        )
                    });
                assert_eq!(found.rowid, Some(rowid));
                assert_eq!(found.payload, row.payload);
            }
            // Rowids that are not there must not be found, including the ones
            // just outside the range in both directions.
            let mut absent: Vec<i64> = vec![i64::MIN, i64::MAX, 0];
            if let Some(first) = present.iter().next() {
                absent.push(first.saturating_sub(1));
            }
            if let Some(last) = present.iter().next_back() {
                absent.push(last.saturating_add(1));
            }
            for candidate in (1..40i64).chain(absent) {
                if present.contains(&candidate) {
                    continue;
                }
                assert!(
                    corpus::seek_rowid(&mut pager, root, candidate, &limits)
                        .unwrap()
                        .is_none(),
                    "{} / {}: rowid {candidate} was found but does not exist",
                    fixture.name,
                    object.name
                );
            }
        }
    }
}

/// A seek that misses must land next to where the key would have been, on
/// whichever side the bias asked for.
#[test]
fn a_missed_seek_lands_where_the_bias_asks() {
    let limits = Limits::default();
    let (_vfs, mut pager) = corpus::open_fixture("deep-p512-utf8.db").unwrap();
    let object = schema::find_object(&mut pager, "many").unwrap().unwrap();
    let root = object.root_page.unwrap();
    let encoding = pager.text_encoding();
    let rows =
        corpus::scan_tree(&mut pager, root, corpus::RootKind::Table, &limits, encoding).unwrap();
    let rowids: Vec<i64> = rows.iter().filter_map(|row| row.rowid).collect();
    assert!(rowids.len() > 1_000);

    // Every rowid is present in this table, so a miss has to be manufactured
    // by seeking outside the range.
    let lowest = *rowids.first().unwrap();
    let highest = *rowids.last().unwrap();

    let mut cursor = BTreeCursor::table(root);
    assert!(!cursor
        .seek_rowid(&mut pager, lowest - 1, SeekBias::AtOrAfter)
        .unwrap());
    assert!(cursor.is_positioned());
    assert_eq!(cursor.rowid().unwrap(), lowest);

    let mut cursor = BTreeCursor::table(root);
    assert!(!cursor
        .seek_rowid(&mut pager, lowest - 1, SeekBias::AtOrBefore)
        .unwrap());
    assert!(
        !cursor.is_positioned(),
        "there is nothing before the first row"
    );

    let mut cursor = BTreeCursor::table(root);
    assert!(!cursor
        .seek_rowid(&mut pager, highest + 1, SeekBias::AtOrBefore)
        .unwrap());
    assert!(cursor.is_positioned());
    assert_eq!(cursor.rowid().unwrap(), highest);

    let mut cursor = BTreeCursor::table(root);
    assert!(!cursor
        .seek_rowid(&mut pager, highest + 1, SeekBias::AtOrAfter)
        .unwrap());
    assert!(
        !cursor.is_positioned(),
        "there is nothing after the last row"
    );
}

/// An index seek must find every key the index holds, and a range scan from a
/// seek must return exactly the entries with that prefix.
#[test]
fn an_index_seek_finds_its_keys_and_bounds_a_range() {
    let limits = Limits::default();
    let (_vfs, mut pager) = corpus::open_fixture("deep-p512-utf8.db").unwrap();
    let index = schema::find_object(&mut pager, "many_by_label")
        .unwrap()
        .unwrap();
    let root = index.root_page.unwrap();
    let encoding = pager.text_encoding();
    let entries =
        corpus::scan_tree(&mut pager, root, corpus::RootKind::Index, &limits, encoding).unwrap();
    assert!(entries.len() > 1_000);

    let key = KeyInfo::binary(1);
    // Every entry the scan found must be reachable by a seek on its own key.
    for entry in entries.iter().step_by(37) {
        let probe = vec![entry.values.first().cloned().unwrap()];
        let mut cursor = BTreeCursor::index(root, key.clone());
        let found = cursor
            .seek_index(&mut pager, &probe, SeekBias::AtOrAfter)
            .unwrap();
        assert!(found, "the index did not find {:?}", probe);
        let payload = cursor.payload(&mut pager, &limits).unwrap();
        assert_eq!(payload, entry.payload);
        cursor.reset();
    }

    // A prefix probe must bound a range: everything from the first entry whose
    // label starts with the prefix, up to the first one that does not.
    let prefix = Value::owned_text(b"row-0001").unwrap();
    let mut cursor = BTreeCursor::index(root, key.clone());
    cursor
        .seek_index(
            &mut pager,
            std::slice::from_ref(&prefix),
            SeekBias::AtOrAfter,
        )
        .unwrap();
    let mut matched = 0usize;
    while cursor.is_positioned() {
        let payload = cursor.payload(&mut pager, &limits).unwrap();
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits).unwrap();
        let label = record.value(0).unwrap();
        let text = label.as_text().map(|text| text.utf8_bytes().into_owned());
        let Some(text) = text else { break };
        if !text.starts_with(b"row-0001") {
            break;
        }
        matched = matched.saturating_add(1);
        if !cursor.next(&mut pager).unwrap() {
            break;
        }
    }
    cursor.reset();
    // row-000100 .. row-000199 plus row-000100's own prefix row-0001xx set.
    assert_eq!(matched, 100, "the prefix range returned {matched} entries");
}

/// An overflow payload must be reassembled byte for byte, including the ones
/// that sit exactly on the local-payload threshold.
#[test]
fn overflow_payloads_are_reassembled_exactly() {
    let (_vfs, mut pager) = corpus::open_fixture("overflow-p512-utf8.db").unwrap();
    let scanned = corpus::scan_database(&mut pager).unwrap();
    let rows = scanned.table("payloads").expect("the payloads table");
    assert_eq!(rows.len(), 8);

    let expected_lengths = [50usize, 200, 230, 239, 240, 1_000, 20_000, 100_000];
    for (row, expected) in rows.iter().zip(expected_lengths) {
        let body = row.values.get(1).and_then(|value| value.as_text());
        let raw = row.values.get(2).and_then(|value| value.as_blob());
        let body = body.expect("a text body");
        let raw = raw.expect("a blob body");
        // `hex(zeroblob(n))` is 2n characters of '0'.
        assert_eq!(
            body.len(),
            expected.saturating_mul(2),
            "row {:?} has the wrong text length",
            row.rowid
        );
        assert!(body.raw().iter().all(|byte| *byte == b'0'));
        assert_eq!(
            raw.len(),
            expected,
            "row {:?} has the wrong blob",
            row.rowid
        );
        assert!(raw.raw().iter().all(|byte| *byte == 0));
    }

    // The long-key table puts an index key over the threshold, which is the
    // case where the *index* page, not a table leaf, has to overflow.
    let keys = scanned.table("long_keys").expect("the long_keys table");
    assert_eq!(keys.len(), 3);
}

/// Every valid fixture must pass both levels of integrity check.
#[test]
fn every_valid_fixture_passes_the_integrity_check() {
    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        let objects = schema::load_schema(&mut pager).unwrap();
        let keys = corpus::index_key_map(&objects);
        let quick = check::check_database_with_keys(&mut pager, CheckLevel::Quick, &keys).unwrap();
        assert!(
            quick.is_ok(),
            "{} failed the quick check: {:?}",
            fixture.name,
            quick.problems
        );
        let full =
            check::check_database_with_keys(&mut pager, CheckLevel::Integrity, &keys).unwrap();
        assert!(
            full.is_ok(),
            "{} failed the integrity check: {:?}",
            fixture.name,
            full.problems
        );
        assert!(
            full.tree_pages >= 1,
            "{} reported no tree pages",
            fixture.name
        );
    }
}

/// The vacuum fixtures must be recognised as such, and their pointer-map pages
/// must be accounted for rather than reported as unreachable.
#[test]
fn the_vacuum_fixtures_have_pointer_maps_that_the_check_accounts_for() {
    for (name, expected) in [
        (
            "autovacuum-p1024-utf8.db",
            inillucent_storage::VacuumMode::Auto,
        ),
        (
            "incrvacuum-p1024-utf8.db",
            inillucent_storage::VacuumMode::Incremental,
        ),
        ("basic-p1024-utf8.db", inillucent_storage::VacuumMode::None),
    ] {
        let (_vfs, mut pager) = corpus::open_fixture(name).unwrap();
        assert_eq!(pager.header().vacuum_mode, expected, "{name}");
        let objects = schema::load_schema(&mut pager).unwrap();
        let keys = corpus::index_key_map(&objects);
        let report =
            check::check_database_with_keys(&mut pager, CheckLevel::Integrity, &keys).unwrap();
        assert!(report.is_ok(), "{name}: {:?}", report.problems);
        if expected == inillucent_storage::VacuumMode::None {
            assert_eq!(report.pointer_map_pages, 0, "{name}");
        } else {
            assert!(report.pointer_map_pages > 0, "{name} has no pointer map");
        }
    }
}

/// The freelist fixture must have a freelist, and the check must find exactly
/// as many pages on it as the header claims.
#[test]
fn the_freelist_fixture_has_a_freelist_the_check_agrees_with() {
    let (_vfs, mut pager) = corpus::open_fixture("freelist-p1024-utf8.db").unwrap();
    let claimed = pager.header().freelist_count;
    assert!(claimed > 0, "the fixture has no freelist");
    let report = check::check_database(&mut pager, CheckLevel::Integrity).unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
    assert_eq!(report.freelist_pages, u64::from(claimed));
}

/// A WITHOUT ROWID table is an index B-tree used as a table, and must scan.
#[test]
fn a_without_rowid_table_scans_as_an_index_tree() {
    let (_vfs, mut pager) = corpus::open_fixture("withoutrowid-p1024-utf8.db").unwrap();
    let scanned = corpus::scan_database(&mut pager).unwrap();
    let kv = scanned.table("kv").expect("the kv table");
    assert_eq!(kv.len(), 5);
    // A WITHOUT ROWID table stores the whole row in the index key, in the
    // primary key's order, so the first column is the sorted key.
    let keys: Vec<String> = kv
        .iter()
        .filter_map(|row| row.values.first())
        .filter_map(|value| value.as_text())
        .map(|text| String::from_utf8_lossy(text.utf8_bytes().as_ref()).into_owned())
        .collect();
    assert_eq!(keys, vec!["", "alpha", "bravo", "charlie", "zulu"]);

    let pair = scanned.table("pair").expect("the pair table");
    assert_eq!(pair.len(), 3);
    // A two-column primary key sorts by the first column and then the second.
    let ordered: Vec<(i64, String)> = pair
        .iter()
        .map(|row| {
            (
                row.values
                    .first()
                    .and_then(|value| value.as_integer())
                    .unwrap(),
                row.values
                    .get(1)
                    .and_then(|value| value.as_text())
                    .map(|text| String::from_utf8_lossy(text.utf8_bytes().as_ref()).into_owned())
                    .unwrap(),
            )
        })
        .collect();
    assert_eq!(
        ordered,
        vec![
            (1, "x".to_string()),
            (1, "y".to_string()),
            (2, "x".to_string())
        ]
    );
}

/// The collation fixture's three indexes must each be sorted under their own
/// collation and not under the others.
#[test]
fn each_collation_index_is_sorted_under_its_own_collation() {
    let (_vfs, mut pager) = corpus::open_fixture("collations-p1024-utf8.db").unwrap();
    let encoding = pager.text_encoding();
    let scanned = corpus::scan_database(&mut pager).unwrap();
    for (name, collation) in [
        ("words_binary", Collation::Binary),
        ("words_nocase", Collation::NoCase),
        ("words_rtrim", Collation::RTrim),
    ] {
        let entries = scanned.index(name).unwrap_or_else(|| panic!("{name}"));
        assert!(!entries.is_empty());
        let key = KeyInfo {
            columns: vec![inillucent_value::KeyColumn {
                collation,
                descending: false,
            }],
        };
        let mut previous: Option<Vec<u8>> = None;
        for entry in entries {
            if let Some(before) = previous.as_ref() {
                let left =
                    RecordRef::parse_with_limits(before, encoding, &Limits::default()).unwrap();
                let right =
                    RecordRef::parse_with_limits(&entry.payload, encoding, &Limits::default())
                        .unwrap();
                // Only the first column is compared under the collation; the
                // trailing rowid breaks ties, so a tie under the collation is
                // still ordered by the rowid and never reversed.
                let ordering = record::compare_records(&left, &right, &key).unwrap();
                assert_ne!(
                    ordering,
                    std::cmp::Ordering::Greater,
                    "{name} is not sorted under {collation:?}"
                );
            }
            previous = Some(entry.payload.clone());
        }
    }
}

/// Every malformed fixture must be refused, and refused as corruption rather
/// than as a panic, a hang, or a plausible-looking answer.
///
/// The one entry in the corpus that SQLite does *not* refuse is checked the
/// other way round: inillucent must read it as cleanly as SQLite does, because
/// refusing a file SQLite opens is a parity failure rather than a safety win.
#[test]
fn every_malformed_fixture_is_refused() {
    let vfs = OsVfs::new();
    for fixture in malformed_fixtures() {
        let path = DbPath::new(corpus::fixture_path(fixture.name));
        if !fixture.refused_by_sqlite {
            let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default())
                .unwrap_or_else(|error| {
                    panic!("{} ({}) must open: {error}", fixture.name, fixture.lie)
                });
            pager.begin_read().unwrap();
            let scanned = corpus::scan_database(&mut pager).unwrap_or_else(|error| {
                panic!("{} ({}) must scan: {error}", fixture.name, fixture.lie)
            });
            assert!(!scanned.tables.is_empty(), "{}", fixture.name);
            let objects = schema::load_schema(&mut pager).unwrap();
            let keys = corpus::index_key_map(&objects);
            let report =
                check::check_database_with_keys(&mut pager, CheckLevel::Integrity, &keys).unwrap();
            assert!(
                report.is_ok(),
                "{} ({}) must check clean: {:?}",
                fixture.name,
                fixture.lie,
                report.problems
            );
            continue;
        }
        let opened = Pager::open_read_only(&vfs, &path, PagerOptions::default());
        match opened {
            Err(error) => {
                assert!(
                    fixture.fails_at_open,
                    "{} failed at open but was not expected to: {error}",
                    fixture.name
                );
                assert!(
                    matches!(
                        error.code(),
                        PrimaryCode::Corrupt | PrimaryCode::NotADb | PrimaryCode::IoErr
                    ),
                    "{} failed with {:?} rather than a corruption code",
                    fixture.name,
                    error.code()
                );
            }
            Ok(mut pager) => {
                assert!(
                    !fixture.fails_at_open,
                    "{} opened but was expected to fail at open",
                    fixture.name
                );
                pager.begin_read().unwrap();
                // Something in the walk must refuse it: either the scan errors,
                // or the integrity check reports a problem. What must not
                // happen is a clean scan of a file we deliberately broke.
                let scan = corpus::scan_database(&mut pager);
                let clean_scan = scan.is_ok();
                pager.clear_sticky_error();
                let report = check::check_database(&mut pager, CheckLevel::Integrity);
                let clean_check = report
                    .as_ref()
                    .map(|report| report.is_ok())
                    .unwrap_or(false);
                assert!(
                    !(clean_scan && clean_check),
                    "{} ({}) was read cleanly",
                    fixture.name,
                    fixture.lie
                );
                if let Err(error) = scan {
                    assert!(
                        matches!(
                            error.code(),
                            PrimaryCode::Corrupt | PrimaryCode::TooBig | PrimaryCode::IoErr
                        ),
                        "{} failed with {:?}",
                        fixture.name,
                        error.code()
                    );
                }
            }
        }
    }
}

/// Reading a database must change no byte of it, no byte of any sibling file,
/// and must create no journal, WAL, or shared-memory file.
#[test]
fn reading_changes_nothing_on_disk() {
    let directory = corpus::corpus_dir();
    let before = corpus::directory_snapshot(&directory).unwrap();

    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        let _ = corpus::scan_database(&mut pager).unwrap();
        let _ = check::check_database(&mut pager, CheckLevel::Integrity).unwrap();
        pager.end_read().unwrap();
        pager.close().unwrap();
    }

    let after = corpus::directory_snapshot(&directory).unwrap();
    assert_eq!(
        before.len(),
        after.len(),
        "reading created or removed a file in the corpus"
    );
    for (left, right) in before.iter().zip(after.iter()) {
        assert_eq!(left, right, "reading changed {}", left.0);
    }
    for (name, _, _) in &after {
        assert!(
            !name.ends_with("-journal") && !name.ends_with("-wal") && !name.ends_with("-shm"),
            "reading left {name} behind"
        );
    }
}

/// A full traversal must return every pin it took, and leave the lock where it
/// found it. A cursor that leaked a pin would slowly wedge the cache.
#[test]
fn a_traversal_returns_every_pin_and_lock() {
    for fixture in valid_fixtures() {
        let (_vfs, mut pager) = corpus::open_fixture(fixture.name).unwrap();
        assert_eq!(pager.lock_level(), inillucent_vfs::FileLock::Shared);
        let _ = corpus::scan_database(&mut pager).unwrap();
        let _ = check::check_database(&mut pager, CheckLevel::Integrity).unwrap();
        assert_eq!(
            pager.cache().pinned_frames(),
            0,
            "{} leaked a pin after a scan",
            fixture.name
        );
        pager.end_read().unwrap();
        assert_eq!(pager.lock_level(), inillucent_vfs::FileLock::None);
        assert_eq!(
            pager.cache_counters().resident,
            0,
            "{} left pages resident after the read ended",
            fixture.name
        );
    }
}

/// Cache pressure must not change any answer, only how often the file is read.
#[test]
fn a_tiny_cache_returns_the_same_rows_as_a_large_one() {
    let vfs = OsVfs::new();
    let path = DbPath::new(corpus::fixture_path("deep-p512-utf8.db"));

    let mut roomy = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
    roomy.begin_read().unwrap();
    let expected = corpus::scan_database(&mut roomy).unwrap();

    // Four pages of room, against a file of two hundred.
    let cramped_options = PagerOptions {
        cache_bytes: 512 * 4,
        ..PagerOptions::default()
    };
    let mut cramped = Pager::open_read_only(&vfs, &path, cramped_options).unwrap();
    cramped.begin_read().unwrap();
    let actual = corpus::scan_database(&mut cramped).unwrap();

    assert_eq!(expected.tables.len(), actual.tables.len());
    for ((left_name, left_rows), (right_name, right_rows)) in
        expected.tables.iter().zip(actual.tables.iter())
    {
        assert_eq!(left_name, right_name);
        assert_eq!(left_rows.len(), right_rows.len());
        for (left, right) in left_rows.iter().zip(right_rows.iter()) {
            assert_eq!(left.payload, right.payload);
        }
    }
    assert!(
        cramped.cache_counters().evictions > 0,
        "the cramped cache never evicted anything"
    );

    // A sequential scan is almost immune to cache pressure, and it is worth
    // saying why rather than asserting something that is not true: the cursor
    // pins its whole root-to-leaf path, so the ancestors cannot be evicted
    // while it is below them and each leaf is read exactly once either way.
    // Repeated point seeks are the workload that shows the difference,
    // because each one descends from the root again with nothing pinned in
    // between.
    let object = schema::find_object(&mut roomy, "many").unwrap().unwrap();
    let root = object.root_page.unwrap();
    let limits = Limits::default();
    let before_roomy = roomy.counters().page_reads;
    let before_cramped = cramped.counters().page_reads;
    for step in 0..300i64 {
        let rowid = step.saturating_mul(7).saturating_add(1);
        let from_roomy = corpus::seek_rowid(&mut roomy, root, rowid, &limits).unwrap();
        let from_cramped = corpus::seek_rowid(&mut cramped, root, rowid, &limits).unwrap();
        assert_eq!(
            from_roomy.map(|row| row.payload),
            from_cramped.map(|row| row.payload),
            "the two caches disagree about rowid {rowid}"
        );
    }
    let roomy_reads = roomy.counters().page_reads.saturating_sub(before_roomy);
    let cramped_reads = cramped.counters().page_reads.saturating_sub(before_cramped);
    assert!(
        cramped_reads > roomy_reads.saturating_mul(2),
        "300 seeks read {cramped_reads} pages under pressure and {roomy_reads} without it"
    );
    assert_eq!(cramped.cache().pinned_frames(), 0);
    assert_eq!(roomy.cache().pinned_frames(), 0);
}

/// Two readers on the same file must both work, and neither must see the other.
#[test]
fn two_readers_see_the_same_database() {
    let vfs = OsVfs::new();
    let path = DbPath::new(corpus::fixture_path("basic-p4096-utf8.db"));
    let mut first = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
    let mut second = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
    first.begin_read().unwrap();
    second.begin_read().unwrap();
    let left = corpus::scan_database(&mut first).unwrap();
    let right = corpus::scan_database(&mut second).unwrap();
    assert_eq!(left.tables.len(), right.tables.len());
    for ((left_name, left_rows), (right_name, right_rows)) in
        left.tables.iter().zip(right.tables.iter())
    {
        assert_eq!(left_name, right_name);
        assert_eq!(left_rows.len(), right_rows.len());
    }
}

/// A page that is not part of any tree must still be refusable by number, and
/// a page past the end must never be readable.
#[test]
fn a_page_past_the_end_is_never_readable() {
    let (_vfs, mut pager) = corpus::open_fixture("basic-p4096-utf8.db").unwrap();
    let count = pager.page_count();
    let error = pager
        .get_page(PageId::from_persisted(count.saturating_add(1)).unwrap())
        .unwrap_err();
    assert_eq!(error.code(), PrimaryCode::Corrupt);
    assert!(pager.get_page(PageId::from_persisted(1).unwrap()).is_ok());
}

/// The corpus on disk must be the corpus the generator describes.
///
/// A fixture edited by hand would make every other test in this file a test of
/// something nobody wrote down.
#[test]
fn the_corpus_matches_its_manifest() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_inillucent-fixtures"))
        .arg("--verify")
        .output()
        .expect("the fixture tool runs");
    assert!(
        output.status.success(),
        "the corpus does not match its manifest: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
