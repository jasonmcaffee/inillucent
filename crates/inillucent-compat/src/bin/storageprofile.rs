//! What a storage primitive costs, broken down far enough to act on.
//!
//! Invariant: every step is measured against the step below it, so a number
//! here is a *difference* rather than a total. The read baselines say a rowid
//! point read costs 1.6 microseconds and a page-cache hit costs 32 nanoseconds;
//! between those two numbers is a factor of fifty that nothing in the counters
//! explains, and a factor of fifty is where an optimisation lives. Attributing
//! it needs the seek separated from the payload, the payload from the record,
//! and the record from the column - which is what this does.
//!
//! Everything is resident before the timing starts. This measures work, not
//! I/O; the I/O numbers are in the phase 3 baselines and have not moved.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-storageprofile`

use std::process::ExitCode;
use std::time::Instant;

use inillucent_base::limits::Limits;
use inillucent_base::page::PageSize;
use inillucent_storage::cursor::{BTreeCursor, SeekBias};
use inillucent_storage::mutate;
use inillucent_storage::pager::{NewDatabase, Pager, PagerOptions};
use inillucent_value::record::{encode_record, KeyColumn, KeyInfo, RecordRef};
use inillucent_value::{Collation, TextEncoding, Value};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::DbPath;

/// How many rows the tree holds.
const ROWS: i64 = 20_000;

/// How many operations each measurement runs.
const OPERATIONS: u64 = 200_000;

/// Runs every measurement.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Times a body, returning nanoseconds per iteration.
fn time(iterations: u64, mut body: impl FnMut(u64)) -> f64 {
    for index in 0..1_000 {
        body(index);
    }
    let started = Instant::now();
    for index in 0..iterations {
        body(index);
    }
    started.elapsed().as_secs_f64() * 1e9 / iterations as f64
}

/// Returns the key ordering a one-column index uses.
fn key_info() -> KeyInfo {
    KeyInfo {
        columns: vec![
            KeyColumn {
                collation: Collation::Binary,
                descending: false,
            },
            KeyColumn {
                collation: Collation::Binary,
                descending: false,
            },
        ],
    }
}

/// Builds a table and an index over `ROWS` rows, entirely in memory.
fn build() -> Result<
    (
        Pager,
        inillucent_base::ids::PageId,
        inillucent_base::ids::PageId,
    ),
    String,
> {
    let vfs = MemoryVfs::new();
    let mut pager = Pager::create(
        &vfs,
        &DbPath::from("/profile.db"),
        PagerOptions {
            cache_bytes: 256 * 1024 * 1024,
            ..PagerOptions::default()
        },
        NewDatabase {
            page_size: PageSize::DEFAULT,
            ..NewDatabase::default()
        },
    )
    .map_err(|error| error.message().to_string())?;
    pager
        .begin_write()
        .map_err(|error| error.message().to_string())?;
    let table_root =
        mutate::create_table(&mut pager).map_err(|error| error.message().to_string())?;
    let index_root =
        mutate::create_index(&mut pager).map_err(|error| error.message().to_string())?;
    for rowid in 1..=ROWS {
        let values = vec![
            Value::Null,
            Value::Integer(rowid * 7 % ROWS),
            Value::owned_text(format!("row {rowid} lorem ipsum dolor sit amet").as_bytes())
                .map_err(|error| error.message().to_string())?,
            Value::Integer(rowid % 64),
        ];
        let payload = encode_record(&values, TextEncoding::Utf8, 4)
            .map_err(|error| error.message().to_string())?;
        mutate::insert_row(&mut pager, table_root, rowid, &payload)
            .map_err(|error| error.message().to_string())?;
        let entry = encode_record(
            &[Value::Integer(rowid * 7 % ROWS), Value::Integer(rowid)],
            TextEncoding::Utf8,
            4,
        )
        .map_err(|error| error.message().to_string())?;
        mutate::insert_entry(&mut pager, index_root, &key_info(), &entry)
            .map_err(|error| error.message().to_string())?;
    }
    Ok((pager, table_root, index_root))
}

/// Runs and prints every measurement.
fn run() -> Result<(), String> {
    let (mut pager, table_root, index_root) = build()?;
    let limits = Limits::default();
    println!("{ROWS} rows, everything resident, {OPERATIONS} operations each\n");

    let first = inillucent_base::ids::PageId::from_persisted(1).map_err(|_| "page one")?;
    let nanos = time(OPERATIONS, |_| {
        let _ = std::hint::black_box(pager.get_page(first));
    });
    println!("{:>44}  {nanos:>9.1} ns", "get_page (one resident page)");

    let nanos = time(OPERATIONS, |index| {
        let rowid = (index % ROWS as u64) as i64 + 1;
        let mut cursor = BTreeCursor::table(table_root);
        let _ = std::hint::black_box(cursor.seek_rowid(&mut pager, rowid, SeekBias::AtOrAfter));
    });
    println!("{:>44}  {nanos:>9.1} ns", "fresh cursor + seek_rowid");

    let mut held = BTreeCursor::table(table_root);
    let nanos = time(OPERATIONS, |index| {
        let rowid = (index % ROWS as u64) as i64 + 1;
        let _ = std::hint::black_box(held.seek_rowid(&mut pager, rowid, SeekBias::AtOrAfter));
    });
    println!("{:>44}  {nanos:>9.1} ns", "reused cursor + seek_rowid");

    let nanos = time(OPERATIONS, |index| {
        let rowid = (index % ROWS as u64) as i64 + 1;
        let _ = held.seek_rowid(&mut pager, rowid, SeekBias::AtOrAfter);
        let _ = std::hint::black_box(held.payload(&mut pager, &limits));
    });
    println!("{:>44}  {nanos:>9.1} ns", "seek + payload");

    let nanos = time(OPERATIONS, |index| {
        let rowid = (index % ROWS as u64) as i64 + 1;
        let _ = held.seek_rowid(&mut pager, rowid, SeekBias::AtOrAfter);
        let Ok(payload) = held.payload(&mut pager, &limits) else {
            return;
        };
        let Ok(record) = RecordRef::parse(&payload, TextEncoding::Utf8) else {
            return;
        };
        let _ = std::hint::black_box(record.value(2));
    });
    println!(
        "{:>44}  {nanos:>9.1} ns",
        "seek + payload + parse + one column"
    );

    let mut index_cursor = BTreeCursor::index(index_root, key_info());
    let nanos = time(OPERATIONS, |index| {
        let key = (index % ROWS as u64) as i64;
        let _ = std::hint::black_box(index_cursor.seek_index(
            &mut pager,
            &[Value::Integer(key)],
            SeekBias::AtOrAfter,
        ));
    });
    println!("{:>44}  {nanos:>9.1} ns", "reused cursor + seek_index");

    // Index maintenance, which is what every write pays per index. Each pair
    // deletes an entry and puts it straight back, so the tree is unchanged and
    // the measurement can run as long as it likes.
    let operations = OPERATIONS / 10;
    let nanos = time(operations, |index| {
        let rowid = (index % ROWS as u64) as i64 + 1;
        let Ok(entry) = encode_record(
            &[Value::Integer(rowid * 7 % ROWS), Value::Integer(rowid)],
            TextEncoding::Utf8,
            4,
        ) else {
            return;
        };
        let _ = mutate::delete_entry(&mut pager, index_root, &key_info(), &entry);
        let _ = mutate::insert_entry(&mut pager, index_root, &key_info(), &entry);
    });
    println!(
        "{:>44}  {nanos:>9.1} ns",
        "index delete + reinsert (a pair)"
    );

    let nanos = time(operations, |index| {
        let rowid = (index % ROWS as u64) as i64 + 1;
        let values = vec![
            Value::Null,
            Value::Integer(rowid * 7 % ROWS),
            Value::Integer(rowid % 64),
        ];
        let Ok(payload) = encode_record(&values, TextEncoding::Utf8, 4) else {
            return;
        };
        let _ = mutate::insert_row(&mut pager, table_root, ROWS + 1, &payload);
        let _ = mutate::delete_row(&mut pager, table_root, ROWS + 1);
    });
    println!("{:>44}  {nanos:>9.1} ns", "row insert + delete (a pair)");

    let nanos = time(OPERATIONS, |index| {
        let rowid = (index % ROWS as u64) as i64 + 1;
        let values = vec![
            Value::Null,
            Value::Integer(rowid),
            Value::owned_text(b"row lorem ipsum dolor sit amet").unwrap_or(Value::Null),
            Value::Integer(rowid % 64),
        ];
        let _ = std::hint::black_box(encode_record(&values, TextEncoding::Utf8, 4));
    });
    println!("{:>44}  {nanos:>9.1} ns", "encode one four-column record");

    // What one step of a scan costs, and what each thing built on top of it
    // adds. These are the numbers a per-row cost has to be attributed to.
    let mut walker = BTreeCursor::table(table_root);
    let mut at_end = true;
    let nanos = time(OPERATIONS, |_| {
        if at_end {
            at_end = !walker.first(&mut pager).unwrap_or(false);
            return;
        }
        at_end = !walker.next(&mut pager).unwrap_or(false);
    });
    println!("{:>44}  {nanos:>9.1} ns", "one step of a table scan");

    let mut buffer: Vec<u8> = Vec::new();
    let mut at_end = true;
    let nanos = time(OPERATIONS, |_| {
        if at_end {
            at_end = !walker.first(&mut pager).unwrap_or(false);
        } else {
            at_end = !walker.next(&mut pager).unwrap_or(false);
        }
        if !at_end {
            let _ = walker.payload_into(&mut pager, &limits, &mut buffer);
        }
    });
    println!("{:>44}  {nanos:>9.1} ns", "  ... and reading the row");

    let mut spans = Vec::new();
    let mut at_end = true;
    let nanos = time(OPERATIONS, |_| {
        if at_end {
            at_end = !walker.first(&mut pager).unwrap_or(false);
        } else {
            at_end = !walker.next(&mut pager).unwrap_or(false);
        }
        if !at_end {
            let _ = walker.payload_into(&mut pager, &limits, &mut buffer);
            let _ = RecordRef::parse_into(&buffer, &limits, &mut spans);
        }
    });
    println!("{:>44}  {nanos:>9.1} ns", "  ... and finding its fields");

    let mut at_end = true;
    let nanos = time(OPERATIONS, |_| {
        if at_end {
            at_end = !walker.first(&mut pager).unwrap_or(false);
        } else {
            at_end = !walker.next(&mut pager).unwrap_or(false);
        }
        if !at_end {
            let _ = walker.payload_into(&mut pager, &limits, &mut buffer);
            let Ok(header) = RecordRef::parse_into(&buffer, &limits, &mut spans) else {
                return;
            };
            let record = RecordRef::with_fields(&buffer, &spans, header, TextEncoding::Utf8);
            let _ = std::hint::black_box(record.value(1));
        }
    });
    println!("{:>44}  {nanos:>9.1} ns", "  ... and decoding one integer");

    // Parsing a page is the cost every edit pays again, because publishing a
    // new frame throws the cached layout away. A full index leaf is the worst
    // case and the common one.
    let usable = pager
        .usable_size()
        .map_err(|error| error.message().to_string())?;
    let leaf = {
        let mut cursor = BTreeCursor::index(index_root, key_info());
        let _ = cursor.first(&mut pager);
        cursor.current_page().unwrap_or(index_root)
    };
    let cells = {
        let pin = pager
            .get_page(leaf)
            .map_err(|error| error.message().to_string())?;
        let layout = pin
            .layout(usable)
            .map_err(|error| error.message().to_string())?;
        layout.cell_count
    };
    let nanos = time(OPERATIONS / 10, |_| {
        let Ok(pin) = pager.get_page(leaf) else {
            return;
        };
        let _ = std::hint::black_box(inillucent_storage::btree::PageLayout::parse(
            pin.bytes(),
            leaf,
            usable,
        ));
    });
    println!(
        "{:>44}  {nanos:>9.1} ns  ({cells} cells)",
        "PageLayout::parse of a full index leaf"
    );

    let nanos = time(OPERATIONS / 10, |_| {
        let _ = std::hint::black_box(pager.edit_page(leaf, |raw| {
            // The smallest possible edit: put a byte back where it was.
            if let Some(slot) = raw.first_mut() {
                // Writing the byte back where it was, which is the smallest
                // edit that still dirties the page. Written through
                // `black_box` so the compiler cannot see that it changes
                // nothing and delete the write this is timing.
                *slot = std::hint::black_box(*slot);
            }
            Ok(())
        }));
    });
    println!("{:>44}  {nanos:>9.1} ns", "edit_page with a no-op edit");
    Ok(())
}
