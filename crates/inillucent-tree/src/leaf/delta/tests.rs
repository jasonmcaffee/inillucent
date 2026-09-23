//! The delta area's tests: how it reads, how it merges, and how it refuses.
//!
//! Invariant: **every test here builds its delta area by hand, byte for byte
//! as the layout describes it**, rather than through the writer - so a reader
//! is tested against the layout and not against whatever the writer happens to
//! produce. `with_delta` lays out format 2's directory and `with_format_one_delta`
//! the run of rows format 1 wrote.
//!
//! They were in `leaf.rs`'s own test module until task-2074, which added the
//! directory, format 1's rules and a reference merge to grade `live_order`
//! against, and took that file past the size `policy.rs` holds it to. The delta
//! area is its own module, so its tests are too.

use super::super::layout::align8;
use super::super::*;
use crate::types::{ColumnSpec, PhysicalType};

/// Returns a leaf's live rows by the three rules, read directly.
///
/// A tombstoned sorted row is not live; where the delta area holds a key
/// twice the first entry in the directory wins; and a delta row replaces the
/// sorted row with its key. Written as plainly as those three sentences,
/// with no merge, so that it can grade the merge.
///
/// @param leaf - the leaf
fn live_by_the_rules<'p>(leaf: &LeafRef<'p>) -> Vec<Vec<Datum<'p>>> {
    let mut rows: Vec<Vec<Datum<'p>>> = Vec::new();
    for index in 0..leaf.delta_count() {
        let values: Vec<Datum<'p>> = (0..leaf.column_count())
            .map(|column| leaf.delta_value(index, column).unwrap())
            .collect();
        if !rows
            .iter()
            .any(|held| leaf.compare_keys(held, &values).is_eq())
        {
            rows.push(values);
        }
    }
    for row in 0..leaf.row_count() {
        if leaf.is_tombstoned(row).unwrap() {
            continue;
        }
        let values: Vec<Datum<'p>> = (0..leaf.column_count())
            .map(|column| leaf.value(row, column).unwrap())
            .collect();
        if !rows
            .iter()
            .any(|held| leaf.compare_keys(held, &values).is_eq())
        {
            rows.push(values);
        }
    }
    rows.sort_by(|left, right| leaf.compare_keys(left, right));
    rows
}

/// `live_order` names the rows the three rules name, in key order - over a
/// leaf with tombstones, a delta row for a new key, a delta row that shadows
/// a sorted one and a delta area holding one key twice.
///
/// `live`, `live_source` and every compaction read through `live_order`,
/// so a merge that disagreed with the rules would be a compaction that
/// silently changed a leaf's contents. It used to be graded against `live`,
/// which had its own merge; since task-2074 `live` is `live_order` with the
/// values read out, so it is graded against `live_by_the_rules` instead.
#[test]
fn live_order_agrees_with_the_reference() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
        ColumnSpec::new(PhysicalType::Int64),
    ];
    let builder = LeafBuilder::new(4096, 1, columns.clone(), 1).unwrap();
    let labels: Vec<String> = (0..24).map(|n| format!("row-{n:04}")).collect();
    let rows: Vec<Vec<Datum<'_>>> = (0..24i64)
        .map(|n| {
            vec![
                Datum::Int(n * 2),
                Datum::Text(labels[n as usize].as_bytes()),
                Datum::Int(n * 5),
            ]
        })
        .collect();
    let page = builder.encode(&rows).unwrap();
    // A delta row for a key that is not there, one that shadows a sorted
    // row, a key held twice - the first entry is the newer and wins - and a
    // tombstone over a third sorted row. A writer never leaves a key twice;
    // a page recovery replayed is what the rule is there for.
    let fresh = vec![Datum::Int(7), Datum::Text(b"inserted"), Datum::Int(70)];
    let shadow = vec![Datum::Int(10), Datum::Text(b"replaced"), Datum::Int(99)];
    let newer = vec![Datum::Int(31), Datum::Text(b"newer"), Datum::Int(1)];
    let older = vec![Datum::Int(31), Datum::Text(b"older"), Datum::Int(2)];
    let mut page = with_delta(&page, &[fresh.clone(), shadow.clone(), newer, older]);
    crate::mutate::LeafMut::new(&mut page)
        .unwrap()
        .set_tombstone(3)
        .unwrap();
    let leaf = LeafRef::parse(&page).unwrap();
    let materialised = live_by_the_rules(&leaf);
    assert_eq!(
        materialised.len(),
        25,
        "24 sorted, one tombstoned, two new keys"
    );
    assert_eq!(
        format!("{:?}", leaf.live().unwrap()),
        format!("{materialised:?}"),
        "`live` disagrees with the rules"
    );
    let source = leaf.live_source().unwrap();
    assert_eq!(
        source.len(),
        materialised.len(),
        "live_source named {} rows where live materialised {}",
        source.len(),
        materialised.len()
    );
    for (row, expected) in materialised.iter().enumerate() {
        for (column, want) in expected.iter().enumerate() {
            let got = source.value(row, column);
            assert_eq!(
                format!("{got:?}"),
                format!("{want:?}"),
                "row {row} column {column}"
            );
        }
    }
}

/// `delta_search` answers every probe the way a walk of the directory from
/// its first entry would, including a probe past the last entry, which
/// task-2082 answers from that entry alone.
///
/// The directory holds a key twice, so `Ok` has to be the first of the two,
/// and the probes run from below the first key to above the last, so both
/// ends and every gap between keys are asked about. An area of one row is
/// checked as well, since there the last entry is also the first.
#[test]
fn delta_search_agrees_with_a_walk_for_every_probe() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
    ];
    let builder = LeafBuilder::new(4096, 1, columns, 1).unwrap();
    let page = builder
        .encode(&[vec![Datum::Int(1_000), Datum::Text(b"sorted")]])
        .unwrap();
    let keys: Vec<i64> = vec![2, 4, 4, 9, 10, 15, 40, 41, 77];
    let many: Vec<Vec<Datum<'_>>> = keys
        .iter()
        .map(|key| vec![Datum::Int(*key), Datum::Text(b"delta")])
        .collect();
    let one = vec![vec![Datum::Int(5), Datum::Text(b"only")]];
    for (rows, held) in [(&many, keys.clone()), (&one, vec![5])] {
        let page = with_delta(&page, rows);
        let leaf = LeafRef::parse(&page).unwrap();
        for probe in -1..90i64 {
            let first = held.iter().position(|key| *key >= probe);
            let expected = match first {
                Some(at) if held[at] == probe => Ok(at),
                Some(at) => Err(at),
                None => Err(held.len()),
            };
            assert_eq!(
                leaf.delta_search(&[Datum::Int(probe)]).unwrap(),
                expected,
                "probe {probe} over {held:?}"
            );
        }
    }
}

/// Returns whether a row lies inside a range, by `compare_prefix` alone.
///
/// @param leaf - the leaf whose collations and directions apply
/// @param row - the row
/// @param low - the lower bound and whether it is inclusive
/// @param high - the upper bound and whether it is inclusive
fn inside(
    leaf: &LeafRef<'_>,
    row: &[Datum<'_>],
    low: Option<(&[Datum<'_>], bool)>,
    high: Option<(&[Datum<'_>], bool)>,
) -> bool {
    let above = low.is_none_or(|(bound, inclusive)| match leaf.compare_prefix(row, bound) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => inclusive,
        std::cmp::Ordering::Less => false,
    });
    let below = high.is_none_or(|(bound, inclusive)| match leaf.compare_prefix(row, bound) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => inclusive,
        std::cmp::Ordering::Greater => false,
    });
    above && below
}

/// `live_between` returns exactly the rows the three rules make live that lie
/// inside the bounds, for every pair of bounds over a leaf with tombstones, a
/// shadowing delta row, a key held twice and new keys at both ends.
///
/// task-2082 made it search both regions rather than filter `live`, so it
/// now has its own merge, and a range that lost or doubled a row at an edge
/// would be a range scan returning the wrong rows. Every bound value from
/// below the first key to above the last, in every combination of inclusive
/// and exclusive and open ends, is compared against the rules filtered by
/// `compare_prefix`, which is the filter the old implementation applied.
/// The second leaf has a two-column key and one-column bounds, which is what
/// a range over the leading column of an index asks.
#[test]
fn live_between_agrees_with_the_rules_for_every_bound() {
    let one = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
    ];
    let two = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::key(PhysicalType::Int64),
    ];
    let label = b"sorted".as_slice();
    let rows_one: Vec<Vec<Datum<'_>>> = (0..20i64)
        .map(|n| vec![Datum::Int(n * 3), Datum::Text(label)])
        .collect();
    let rows_two: Vec<Vec<Datum<'_>>> = (0..20i64)
        .map(|n| vec![Datum::Int(n / 3 * 3), Datum::Int(n)])
        .collect();
    let first = LeafBuilder::new(4096, 1, one, 1)
        .unwrap()
        .encode(&rows_one)
        .unwrap();
    let second = LeafBuilder::new(4096, 1, two, 2)
        .unwrap()
        .encode(&rows_two)
        .unwrap();
    let mut first = with_delta(
        &first,
        &[
            vec![Datum::Int(-2), Datum::Text(b"below")],
            vec![Datum::Int(7), Datum::Text(b"fresh")],
            vec![Datum::Int(9), Datum::Text(b"shadow")],
            vec![Datum::Int(31), Datum::Text(b"newer")],
            vec![Datum::Int(31), Datum::Text(b"older")],
            vec![Datum::Int(70), Datum::Text(b"above")],
        ],
    );
    let mut second = with_delta(
        &second,
        &[
            vec![Datum::Int(0), Datum::Int(-1)],
            vec![Datum::Int(6), Datum::Int(7)],
            vec![Datum::Int(6), Datum::Int(100)],
            vec![Datum::Int(40), Datum::Int(0)],
        ],
    );
    for page in [&mut first, &mut second] {
        let mut leaf = crate::mutate::LeafMut::new(page).unwrap();
        leaf.set_tombstone(4).unwrap();
        leaf.set_tombstone(11).unwrap();
    }
    for page in [&first, &second] {
        let leaf = LeafRef::parse(page).unwrap();
        let everything = live_by_the_rules(&leaf);
        let values: Vec<i64> = (-4..75).collect();
        let bounds: Vec<Option<(i64, bool)>> = std::iter::once(None)
            .chain(
                values
                    .iter()
                    .flat_map(|v| [Some((*v, true)), Some((*v, false))]),
            )
            .collect();
        for low in &bounds {
            for high in &bounds {
                let low_key = low.map(|(v, _)| [Datum::Int(v)]);
                let high_key = high.map(|(v, _)| [Datum::Int(v)]);
                let low_pair = low_key
                    .as_ref()
                    .zip(*low)
                    .map(|(k, (_, i))| (k.as_slice(), i));
                let high_pair = high_key
                    .as_ref()
                    .zip(*high)
                    .map(|(k, (_, i))| (k.as_slice(), i));
                let expected: Vec<&Vec<Datum<'_>>> = everything
                    .iter()
                    .filter(|row| inside(&leaf, row, low_pair, high_pair))
                    .collect();
                let got = leaf
                    .live_between(
                        low_pair.map(|(k, _)| k),
                        low_pair.is_none_or(|(_, i)| i),
                        high_pair.map(|(k, _)| k),
                        high_pair.is_none_or(|(_, i)| i),
                    )
                    .unwrap();
                assert_eq!(
                    format!("{got:?}"),
                    format!("{expected:?}"),
                    "live_between({low:?}, {high:?})"
                );
            }
        }
    }
}

/// Writes a delta area into an already-built page.
///
/// Nothing in Phase 1 *writes* a delta area - the leaf builder always
/// leaves it empty and the tree rewrites a leaf rather than appending to
/// one, because the delta path is a Phase 3 write-family item measured
/// against the 16/32/64 sweep. The reader exists now, though, and a reader
/// of bytes that come off a disk is exactly the code that has to be
/// exercised before those bytes are hostile. So the tests build the area by
/// hand, byte for byte as the layout describes it.
///
/// @param page - a page from `LeafBuilder::encode`
/// @param rows - the delta rows, each a list of values in column order
fn with_delta(page: &[u8], rows: &[Vec<Datum<'_>>]) -> Vec<u8> {
    let mut out = page.to_vec();
    let leaf = LeafRef::parse(&out).unwrap();
    let count = leaf.row_count();
    let columns = leaf.column_count();
    let mut end = leaf_header::DIRECTORY + columns * leaf.directory_entry_size();
    for index in 0..columns {
        end = align8(end);
        end += class_bytes(count) + count * leaf.column_width(index).unwrap();
    }
    let encoded: Vec<Vec<u8>> = rows
        .iter()
        .map(|row| {
            let mut bytes = Vec::new();
            for value in row {
                value.encode_tagged(&mut bytes);
            }
            bytes
        })
        .collect();
    // The area sits against the heap: the directory, then the rows in the
    // order the directory names them. The caller hands them in directory
    // order, which is key order for any page a writer produced - and a test
    // of what the integrity check refuses can hand them in any other.
    let heap_start = leaf.heap_start;
    let body: usize = encoded.iter().map(|row| row.len() + 2).sum();
    let delta_start = heap_start - body - DELTA_ENTRY * rows.len();
    // Room for a tombstone bitmap between the mini-columns and the delta
    // area, because that is where the layout puts one and a later
    // `with_tombstones` has to have somewhere to write it.
    assert!(
        align8(end + tombstone_bytes(count)) <= delta_start,
        "the delta does not fit: the columns end at {end} and the area starts at {delta_start}"
    );
    let mut at = delta_start + DELTA_ENTRY * rows.len();
    for (index, row) in encoded.iter().enumerate() {
        let entry = delta_start + DELTA_ENTRY * index;
        page::write_u16(&mut out, entry, (heap_start - at) as u16).unwrap();
        page::write_u16(&mut out, at, row.len() as u16).unwrap();
        out[at + 2..at + 2 + row.len()].copy_from_slice(row);
        at += 2 + row.len();
    }
    page::write_u32(&mut out, leaf_header::DELTA_START, delta_start as u32).unwrap();
    page::write_u16(&mut out, leaf_header::DELTA_COUNT, rows.len() as u16).unwrap();
    out[header::FLAGS] |= LEAF_HAS_DELTA;
    out
}

/// Sets a tombstone bit, moving the delta area up to make room for the
/// bitmap the way a real delete would.
///
/// @param page - a page from `LeafBuilder::encode`
/// @param rows - which sorted-region rows to mark deleted
fn with_tombstones(page: &[u8], rows: &[usize]) -> Vec<u8> {
    let mut out = page.to_vec();
    let leaf = LeafRef::parse(&out).unwrap();
    let count = leaf.row_count();
    let delta_start = leaf.delta_start;
    let bitmap = delta_start - tombstone_bytes(count);
    for row in rows {
        out[bitmap + row / 8] |= 1u8 << (row % 8);
    }
    out[header::FLAGS] |= LEAF_HAS_TOMBSTONES;
    out
}

/// A delta area reads back row by row and value by value, and merges into
/// the live set in key order.
#[test]
fn a_delta_area_reads_back_and_merges() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
    ];
    let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
    let sorted: Vec<Vec<Datum<'static>>> = [10i64, 20, 30]
        .iter()
        .map(|key| {
            vec![
                Datum::Int(*key),
                Datum::Int(key * 2),
                Datum::Text(b"sorted"),
            ]
        })
        .collect();
    let page = builder.encode(&sorted).unwrap();
    // In key order, because the directory is.
    let delta = vec![
        vec![Datum::Int(5), Datum::Null, Datum::Text(b"delta-b")],
        vec![Datum::Int(25), Datum::Int(50), Datum::Text(b"delta-a")],
    ];
    let page = with_delta(&page, &delta);
    let leaf = LeafRef::parse(&page).unwrap();

    assert_eq!(leaf.delta_count(), 2);
    assert!(!leaf.is_clean(), "a delta area leaves the fast path");
    assert_eq!(leaf.live_rows().unwrap(), 5);
    assert_eq!(leaf.delta_value(1, 0).unwrap().as_int(), Some(25));
    assert_eq!(
        leaf.delta_value(1, 2).unwrap().as_bytes(),
        Some(b"delta-a".as_slice())
    );
    assert_eq!(leaf.delta_value(0, 0).unwrap().as_int(), Some(5));
    assert!(leaf.delta_value(0, 1).unwrap().is_null());
    assert!(!leaf.delta_row(1).unwrap().is_empty());
    assert_eq!(leaf.delta_search(&[Datum::Int(25)]).unwrap(), Ok(1));
    assert_eq!(leaf.delta_search(&[Datum::Int(6)]).unwrap(), Err(1));

    // The merge: sorted region and delta together, in key order.
    let live = leaf.live().unwrap();
    let keys: Vec<i64> = live
        .iter()
        .map(|row| row[0].as_int().unwrap_or(-1))
        .collect();
    assert_eq!(keys, vec![5, 10, 20, 25, 30]);
    leaf.integrity().unwrap();
}

/// `locate` decodes each delta row it compares once, left to right, and
/// stops at the first column that differs - it does not re-measure a column
/// it has already read. The rows it compares are the ones the directory's
/// binary search visits (task-2074), and the rule holds for each of them.
///
/// Five delta rows share their first two key columns and differ only on
/// the third, so a probe that agrees with all five on those first two
/// columns forces every row's comparison to walk out to the third before
/// it can be ruled out - the shape that made `locate`'s old per-column
/// `delta_value` calls cost the square of the key's width: comparing
/// column two re-measured column zero's and column one's spans from
/// scratch, on every one of the five rows.
///
/// `Datum::tagged_span` is the call that measured a span it was not about
/// to read - a skip past a column the caller wants no value from - so it
/// is what a re-walk shows up as, and it is a test-only counter
/// (`datum::probe`) rather than a clock, because a call count reads the
/// same on an idle box and a loaded one where a duration would not.
/// Reverting the fix and running only this test - with the counter kept -
/// reads exactly 15: `1 + 2` re-measured spans on each of the five rows.
#[test]
fn locate_stops_reading_a_delta_row_at_the_first_mismatched_column() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::key(PhysicalType::Text),
    ];
    let builder = LeafBuilder::new(8192, 1, columns, 3).unwrap();
    // Sorted so it never collides with the delta rows' key: `999` sorts
    // after every probe or delta key this test uses.
    let sorted = vec![vec![Datum::Int(999), Datum::Int(0), Datum::Text(b"sorted")]];
    let page = builder.encode(&sorted).unwrap();
    let delta = vec![
        vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-0")],
        vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-1")],
        vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-2")],
        vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-3")],
        vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-4")],
    ];
    let page = with_delta(&page, &delta);
    let leaf = LeafRef::parse(&page).unwrap();

    // A hit is still found correctly - the walk is reordered, not the answer.
    crate::datum::probe::reset_tagged_span_calls();
    let found = leaf
        .locate(&[Datum::Int(0), Datum::Int(0), Datum::Text(b"row-2")], 3)
        .unwrap();
    assert_eq!(found, crate::write::Located::Delta(2));

    // A miss that agrees with every row on the first two columns is the
    // case that used to pay for the re-walk five times over.
    crate::datum::probe::reset_tagged_span_calls();
    let missing = leaf
        .locate(&[Datum::Int(0), Datum::Int(0), Datum::Text(b"nomatch")], 3)
        .unwrap();
    assert_eq!(missing, crate::write::Located::Absent);
    assert_eq!(
        crate::datum::probe::tagged_span_calls(),
        0,
        "locate should decode each of the 5 delta rows' 3 columns once, left \
         to right, through decode_tagged - a re-walk that skips a column \
         it is about to decode anyway would show up here as tagged_span \
         calls greater than zero"
    );
}

/// Every way a delta area can be malformed is refused, and none of them
/// panics.
#[test]
fn a_malformed_delta_area_is_refused() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
    ];
    let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
    let sorted = vec![
        vec![Datum::Int(1), Datum::Int(1)],
        vec![Datum::Int(2), Datum::Int(2)],
    ];
    let base = builder.encode(&sorted).unwrap();
    let good = with_delta(&base, &[vec![Datum::Int(7), Datum::Int(7)]]);
    LeafRef::parse(&good).unwrap();

    LeafRef::parse(&good).unwrap().integrity().unwrap();
    // **The rows are checked by `integrity`, not by `parse`** (task-2074).
    // A parse runs on every level of every descent and the area now holds
    // as many rows as the free gap does, so the walk moved to the check that
    // is allowed to cost time in proportion to the page.
    let refused = |page: &[u8]| {
        LeafRef::parse(page)
            .and_then(|leaf| leaf.integrity())
            .is_err()
    };

    // A length that says the row is longer than it is: the values stop
    // decoding before the declared end.
    let leaf = LeafRef::parse(&good).unwrap();
    let at = leaf.delta_offset(0).unwrap();
    let mut lying_length = good.clone();
    page::write_u16(&mut lying_length, at, 40).unwrap();
    assert!(refused(&lying_length));

    // A length that reaches past the heap, which the row accessor refuses
    // as well as the check.
    let mut past_the_heap = good.clone();
    page::write_u16(&mut past_the_heap, at, 60_000).unwrap();
    assert!(refused(&past_the_heap));
    assert!(LeafRef::parse(&past_the_heap)
        .unwrap()
        .delta_row(0)
        .is_err());

    // A tag byte that is not a value.
    let mut bad_tag = good.clone();
    bad_tag[at + 2] = 200;
    assert!(refused(&bad_tag));

    // A row that decodes to fewer bytes than it declared.
    let mut short_row = good.clone();
    page::write_u16(&mut short_row, at, 19).unwrap();
    assert!(refused(&short_row));

    // A directory entry that points into the directory itself, which the
    // offset accessor refuses before anything is read there.
    let mut into_the_directory = good.clone();
    let distance = (leaf.heap_start - leaf.delta_start) as u16;
    page::write_u16(&mut into_the_directory, leaf.delta_start, distance).unwrap();
    assert!(LeafRef::parse(&into_the_directory)
        .unwrap()
        .delta_row(0)
        .is_err());
    assert!(refused(&into_the_directory));

    // A directory too long for the area it opens, which `parse` refuses.
    let mut too_many = good.clone();
    page::write_u16(&mut too_many, leaf_header::DELTA_COUNT, 5_000).unwrap();
    assert!(LeafRef::parse(&too_many).is_err());

    // A directory out of key order, which every binary search over it
    // would misread.
    let backwards = with_delta(
        &base,
        &[
            vec![Datum::Int(9), Datum::Int(9)],
            vec![Datum::Int(7), Datum::Int(7)],
        ],
    );
    assert!(refused(&backwards));

    // Asking for a delta row and a delta column that do not exist.
    let leaf = LeafRef::parse(&good).unwrap();
    assert!(leaf.delta_row(1).is_err());
    assert!(leaf.delta_value(0, 9).is_err());
}

/// A delta row whose key is already live in the sorted region is an
/// integrity failure, because a reader would then see the key twice.
#[test]
fn a_delta_row_may_not_duplicate_a_live_key() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
    ];
    let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
    let sorted = vec![
        vec![Datum::Int(1), Datum::Int(10)],
        vec![Datum::Int(2), Datum::Int(20)],
    ];
    let base = builder.encode(&sorted).unwrap();
    let clashing = with_delta(&base, &[vec![Datum::Int(2), Datum::Int(99)]]);
    let leaf = LeafRef::parse(&clashing).unwrap();
    assert!(leaf.integrity().is_err());

    // Unless the sorted-region row is tombstoned, in which case the delta
    // row is the live one and there is no duplicate.
    let tombstoned = with_tombstones(&clashing, &[1]);
    let leaf = LeafRef::parse(&tombstoned).unwrap();
    leaf.integrity().unwrap();
    assert!(leaf.is_tombstoned(1).unwrap());
    assert!(!leaf.is_tombstoned(0).unwrap());
    assert_eq!(leaf.live_rows().unwrap(), 2);
    let live = leaf.live().unwrap();
    assert_eq!(live.len(), 2);
    assert_eq!(live[1][1].as_int(), Some(99));
}

/// The tombstone bitmap is read only when the flag says it is there, and a
/// row outside it is refused rather than indexed into.
#[test]
fn tombstones_are_read_only_when_they_exist() {
    let columns = vec![ColumnSpec::key(PhysicalType::Int64)];
    let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
    let rows: Vec<Vec<Datum<'static>>> = (0..20).map(|n| vec![Datum::Int(n as i64)]).collect();
    let page = builder.encode(&rows).unwrap();
    let leaf = LeafRef::parse(&page).unwrap();
    assert!(!leaf.has_tombstones());
    assert!(leaf.tombstones().unwrap().is_empty());
    assert!(!leaf.is_tombstoned(0).unwrap());
    assert!(!leaf.is_tombstoned(9999).unwrap(), "no bitmap, no lookup");

    let marked = with_tombstones(&page, &[0, 3, 19]);
    let leaf = LeafRef::parse(&marked).unwrap();
    assert!(leaf.has_tombstones());
    assert!(!leaf.is_clean());
    assert!(!leaf.tombstones().unwrap().is_empty());
    assert!(leaf.is_tombstoned(0).unwrap());
    assert!(!leaf.is_tombstoned(1).unwrap());
    assert!(leaf.is_tombstoned(19).unwrap());
    assert!(leaf.is_tombstoned(20_000).is_err(), "past the bitmap");
    assert_eq!(leaf.live_rows().unwrap(), 17);
    assert_eq!(leaf.live().unwrap().len(), 17);
}

/// Writes a delta area the way format 1 did: no directory, the rows packed
/// against the heap in the order given - which for a format 1 writer was
/// newest first - and the page's format 2 flag cleared.
///
/// @param page - a page from `LeafBuilder::encode`
/// @param rows - the delta rows, newest first
fn with_format_one_delta(page: &[u8], rows: &[Vec<Datum<'_>>]) -> Vec<u8> {
    let mut out = page.to_vec();
    out[header::FLAGS] &= !LEAF_DELTA_DIRECTORY;
    let heap_start = LeafRef::parse(&out).unwrap().heap_start;
    let mut bytes = Vec::new();
    for row in rows {
        let mut encoded = Vec::new();
        for value in row {
            value.encode_tagged(&mut encoded);
        }
        bytes.extend_from_slice(&(encoded.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&encoded);
    }
    let delta_start = heap_start - bytes.len();
    out[delta_start..heap_start].copy_from_slice(&bytes);
    page::write_u32(&mut out, leaf_header::DELTA_START, delta_start as u32).unwrap();
    page::write_u16(&mut out, leaf_header::DELTA_COUNT, rows.len() as u16).unwrap();
    if !rows.is_empty() {
        out[header::FLAGS] |= LEAF_HAS_DELTA;
    }
    out
}

/// A leaf format 1 wrote reads as it always did: its delta area in arrival
/// order, found by a scan, merged into key order, and a key it holds twice
/// answering with the newer row.
///
/// Every database a release before task-2074 wrote is made of such leaves,
/// and `tests/interop/` holds one from each of them; this is the same
/// question asked of a leaf built to hold every case at once.
#[test]
fn a_leaf_format_one_wrote_reads_unchanged() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
    ];
    let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
    let sorted: Vec<Vec<Datum<'static>>> = [10i64, 20, 30, 40]
        .iter()
        .map(|key| vec![Datum::Int(*key), Datum::Text(b"packed")])
        .collect();
    let base = builder.encode(&sorted).unwrap();
    // Newest first, the way format 1 wrote them: 25 twice (the newer says
    // "newer"), and 20 shadowing a packed row.
    let page = with_format_one_delta(
        &base,
        &[
            vec![Datum::Int(25), Datum::Text(b"newer")],
            vec![Datum::Int(5), Datum::Text(b"five")],
            vec![Datum::Int(20), Datum::Text(b"replaced")],
            vec![Datum::Int(25), Datum::Text(b"older")],
        ],
    );
    let leaf = LeafRef::parse(&page).unwrap();
    assert!(!leaf.has_delta_directory());
    assert_eq!(leaf.delta_rows_start(), leaf.delta_start);
    assert_eq!(
        leaf.delta_value(3, 1).unwrap().as_bytes(),
        Some(b"older".as_slice())
    );
    let live = leaf.live().unwrap();
    let read: Vec<(i64, Vec<u8>)> = live
        .iter()
        .map(|row| {
            (
                row[0].as_int().unwrap(),
                row[1].as_bytes().unwrap().to_vec(),
            )
        })
        .collect();
    assert_eq!(
        read,
        vec![
            (5, b"five".to_vec()),
            (10, b"packed".to_vec()),
            (20, b"replaced".to_vec()),
            (25, b"newer".to_vec()),
            (30, b"packed".to_vec()),
            (40, b"packed".to_vec()),
        ]
    );
    assert_eq!(
        format!("{live:?}"),
        format!("{:?}", live_by_the_rules(&leaf))
    );
    assert_eq!(
        leaf.locate(&[Datum::Int(25)], 1).unwrap(),
        crate::write::Located::Delta(0),
        "the newer of the two rows for 25"
    );
    assert_eq!(leaf.delta_search(&[Datum::Int(5)]).unwrap(), Ok(1));
    assert_eq!(leaf.delta_matching(&[Datum::Int(25)]).unwrap(), vec![0, 3]);
    assert!(leaf.holds_a_key_past(&[Datum::Int(24)]).unwrap());
    assert!(!leaf.holds_a_key_past(&[Datum::Int(25)]).unwrap());
    // Two rows for one key is what the integrity check calls a duplicate
    // only against the sorted region; the area's own order is not checked.
    leaf.validate_delta().unwrap();
}

/// A leaf format 1 wrote is written by format 1's rules until something
/// repacks it: a new row goes first, there is no directory, and the 33rd
/// row is refused, which is where a format 1 writer compacted.
///
/// That is what makes a format 1 log replay onto the bytes its writer
/// produced - see `mutate.rs`'s module note.
#[test]
fn a_leaf_format_one_wrote_takes_rows_by_format_one_rules() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
    ];
    let builder = LeafBuilder::new(65_536, 1, columns.clone(), 1).unwrap();
    let sorted: Vec<Vec<Datum<'static>>> = (0..10i64)
        .map(|key| vec![Datum::Int(key * 100), Datum::Int(key)])
        .collect();
    let mut page = with_format_one_delta(&builder.encode(&sorted).unwrap(), &[]);
    let heap_start = LeafRef::parse(&page).unwrap().heap_start;
    let mut leaf = crate::mutate::LeafMut::new(&mut page).unwrap();
    for round in 0..FORMAT_ONE_DELTA_LIMIT as i64 {
        assert_eq!(
            leaf.insert_delta(&columns, &[Datum::Int(1_000 - round), Datum::Int(round)])
                .unwrap(),
            crate::mutate::Applied::Yes,
            "row {round}"
        );
    }
    assert_eq!(
        leaf.insert_delta(&columns, &[Datum::Int(5_000), Datum::Int(0)])
            .unwrap(),
        crate::mutate::Applied::NoRoom,
        "format 1 held at most 32 delta rows"
    );
    let view = leaf.view().unwrap();
    assert!(
        !view.has_delta_directory(),
        "the leaf was converted in place"
    );
    // The newest row sits at `delta_start`, and the rows fill the area to
    // the heap with nothing between them - no directory.
    assert_eq!(view.delta_value(0, 1).unwrap().as_int(), Some(31));
    assert_eq!(view.delta_offset(0).unwrap(), view.delta_start);
    let rows: usize = (0..view.delta_count())
        .map(|index| view.delta_row(index).unwrap().len() + 2)
        .sum();
    assert_eq!(heap_start - view.delta_start, rows);
    view.integrity().unwrap();
}

/// Recovery compacts a leaf format 1 wrote by format 1's rule: a repack,
/// left in format 1's layout, so the rest of a format 1 log applies to the
/// page that log was written against.
#[test]
fn a_format_one_compaction_replays_as_a_format_one_page() {
    let columns = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
    ];
    let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
    let sorted: Vec<Vec<Datum<'static>>> = [10i64, 20, 30]
        .iter()
        .map(|key| vec![Datum::Int(*key), Datum::Text(b"packed")])
        .collect();
    let page = with_format_one_delta(
        &builder.encode(&sorted).unwrap(),
        &[vec![Datum::Int(15), Datum::Text(b"delta")]],
    );
    let leaf = LeafRef::parse(&page).unwrap();
    let image = crate::write::replay_compaction(&builder, &leaf, 8192)
        .unwrap()
        .expect("four rows fit");
    let packed = LeafRef::parse(&image).unwrap();
    assert!(!packed.has_delta_directory());
    assert_eq!(
        splices_of(&packed),
        0,
        "a format 1 leaf is never spliced on replay"
    );
    assert_eq!(packed.row_count(), 4);
    assert_eq!(packed.delta_count(), 0);
    // Byte for byte what format 1's builder wrote, which is this build's
    // with the flag cleared.
    let mut expected = builder
        .pack_all_rows(&leaf.live_source().unwrap(), 0.95)
        .unwrap()
        .unwrap();
    expected[header::FLAGS] &= !LEAF_DELTA_DIRECTORY;
    assert_eq!(image, expected);
}
