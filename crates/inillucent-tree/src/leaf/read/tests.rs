//! Tests of taking a value back out of a leaf page.
//!
//! Invariant: **`LeafRef::column` answers exactly what the four directory
//! accessors answer**, on a page whose entries carry a base and on one whose
//! entries do not. task-2091 made `column` read its directory entry once
//! rather than call `spec`, `column_width`, `column_base` and `column_offset`
//! in turn, and the accessors are what it is graded against.
//!
//! The test is here rather than in `leaf.rs`'s own test module because that
//! file is held to a size by `policy.rs`, and this is the module the code it
//! checks lives in.

use super::super::layout::class_bytes;
use super::super::*;
use crate::page;
use crate::types::{ColumnSpec, PhysicalType};

/// Encodes a leaf of an integer key, an integer and a text column.
///
/// @param keys - the key of each row; the second column is twice the key and
///   the third its decimal text
fn leaf_of(keys: &[i64]) -> Vec<u8> {
    let builder = LeafBuilder::new(
        8192,
        1,
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ],
        1,
    )
    .unwrap();
    let labels: Vec<String> = keys.iter().map(|key| format!("label {key}")).collect();
    let rows: Vec<Vec<Datum<'_>>> = keys
        .iter()
        .zip(&labels)
        .map(|(key, label)| {
            vec![
                Datum::Int(*key),
                Datum::Int(key * 2),
                Datum::Text(label.as_bytes()),
            ]
        })
        .collect();
    builder.encode(&rows).unwrap()
}

/// `column` gives what `spec`, `column_width`, `column_base` and
/// `column_offset` give, for every column of a page with a base per column and
/// of a page without one.
#[test]
fn a_column_reads_its_entry_the_way_the_accessors_do() {
    // Keys far from zero take a base; keys that start at zero do not.
    let framed_keys: Vec<i64> = (0..200).map(|n| 900_000 + n).collect();
    let plain_keys: Vec<i64> = (0..200).collect();
    let framed = leaf_of(&framed_keys);
    let plain = leaf_of(&plain_keys);
    if NARROW_INT_SLOTS && FRAME_OF_REFERENCE {
        let leaf = LeafRef::parse(&framed).unwrap();
        assert_eq!(leaf.directory_entry_size(), 16);
        assert_eq!(leaf.column_base(0).unwrap(), 900_000);
    }
    assert_eq!(LeafRef::parse(&plain).unwrap().directory_entry_size(), 8);
    for page in [&framed, &plain] {
        let leaf = LeafRef::parse(page).unwrap();
        for index in 0..leaf.column_count() {
            let column = leaf.column(index).unwrap();
            let spec = leaf.spec(index).unwrap();
            assert_eq!(column.physical, spec.physical, "column {index}");
            assert_eq!(column.flags, spec.flags, "column {index}");
            assert_eq!(column.width, leaf.column_width(index).unwrap());
            assert_eq!(column.base, leaf.column_base(index).unwrap());
            let values_at = leaf
                .column_offset(index)
                .unwrap()
                .saturating_add(class_bytes(leaf.row_count()));
            assert_eq!(
                column.inline_bytes().as_ptr(),
                page[values_at..].as_ptr(),
                "column {index}'s values start where its entry says"
            );
        }
        for row in [0usize, 57, 199] {
            assert_eq!(
                leaf.column(0).unwrap().value(row).unwrap().as_int(),
                leaf.value(row, 0).unwrap().as_int(),
                "row {row}"
            );
        }
        assert!(leaf.column(3).is_err(), "a column past the directory");
    }
}

/// `column` refuses an entry whose width its type does not admit, as
/// `column_width` does.
#[test]
fn a_column_refuses_a_width_its_type_does_not_admit() {
    // An Int64 slot is 1, 2, 4 or 8 bytes wide and never 3.
    let mut lying = leaf_of(&[1, 2, 3]);
    page::write_u16(&mut lying, leaf_header::DIRECTORY + 2, 3).unwrap();
    let leaf = LeafRef::parse(&lying).unwrap();
    assert!(leaf.column(0).is_err());
    assert!(leaf.column_width(0).is_err());
}
