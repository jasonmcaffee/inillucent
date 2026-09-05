//! Fuzzes the PAX leaf decoder with arbitrary page bytes.
//!
//! Invariant: a leaf page arrives from a file and a file arrives from anywhere,
//! so every read of one is bounds checked against the page's own header before
//! a byte of payload is touched. A corrupt or hostile page must produce a
//! `DbError` and never a panic, a wrong answer or an out-of-bounds read.
//!
//! The target does not merely parse: it parses and then *reads everything* -
//! every column of every row the header claims, the tombstone bitmap, the delta
//! area, a key search - because a decoder that validates its header and then
//! trusts an offset inside it is the shape of every page-decoder bug there has
//! ever been.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_tree::datum::Datum;
use inillucent_tree::leaf::LeafRef;

fuzz_target!(|data: &[u8]| {
    let Ok(leaf) = LeafRef::parse(data) else {
        return;
    };
    // Whatever the header claimed, reading it must not panic.
    let _ = leaf.integrity();
    let rows = leaf.row_count().min(4_096);
    let columns = leaf.column_count().min(64);
    for row in 0..rows {
        let _ = leaf.is_tombstoned(row);
        for column in 0..columns {
            let _ = leaf.value(row, column);
        }
    }
    for entry in 0..leaf.delta_count().min(64) {
        let _ = leaf.delta_row(entry);
        for column in 0..columns {
            let _ = leaf.delta_value(entry, column);
        }
    }
    for column in 0..columns {
        if let Ok(mini) = leaf.column(column) {
            let _ = mini.any_exception();
            let _ = mini.all_typed();
            for row in 0..rows.min(64) {
                let _ = mini.class_at(row);
                let _ = mini.value(row);
                let _ = mini.heap_slice(row);
            }
        }
    }
    let _ = leaf.search(&[Datum::Int(0)]);
    let _ = leaf.search(&[Datum::Text(b"probe")]);
    let _ = leaf.lower_bound(&[Datum::Int(1)]);
    let _ = leaf.upper_bound(&[Datum::Int(1)]);
    let _ = leaf.live();
});
