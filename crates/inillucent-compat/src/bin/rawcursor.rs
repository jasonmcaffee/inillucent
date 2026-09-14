//! What a full scan costs with no VM and no `Value` in the way.
//!
//! Invariant: the aggregate this loop computes equals the one SQLite computes
//! over the same file, on every page size and both b-tree structures. A timing
//! is only worth reading if both sides did the same work and got the same
//! answer, so the aggregate is printed rather than discarded and every round is
//! checked against the warm-up round before its time is kept.
//!
//! Fable 5.1's rearchitecture proposal rests on a claim that can be falsified
//! in an hour: that a batched scan over a storage layer can beat SQLite by 3x
//! or more on a full table scan, and its stage-1 go/no-go bar is 5x on
//! `read.analytical`. This binary measures the ceiling that claim needs.
//!
//! The loop here is the most favourable thing the current storage layer can
//! possibly do. It walks the table b-tree with `BTreeCursor` directly, reads
//! each row's payload into one reused buffer, parses the record header into one
//! reused span vector, and decodes only the two integer fields the aggregate
//! needs, straight out of the borrowed bytes. There is no bytecode, no register
//! file, no `Value`, no per-row allocation and no result-row marshalling - none
//! of the things a real query would still have to pay for.
//!
//! So the number it prints is an UPPER BOUND, not a projection. Whatever this
//! measures, a real engine built on this storage layer is slower. If the upper
//! bound does not clear 3x, no amount of work above the storage layer reaches
//! it, and the bottleneck is the page cache and the cursor rather than the VM.
//!
//! Usage: inillucent-rawcursor `<database>` `rounds`
//!
//! It prints ns per scan and ns per row, and the aggregate it computed, so the
//! answer can be checked against `sqlite3` on the same file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_storage::cursor::BTreeCursor;
use inillucent_storage::pager::Pager;
use inillucent_transaction::recovery::{open_database, DatabaseOptions};
use inillucent_value::record::{FieldSpan, KeyInfo, RecordRef};
use inillucent_value::TextEncoding;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::{OsVfs, Vfs};

/// What one scan produced, kept so the aggregate can be checked against
/// `sqlite3` rather than trusted.
struct Aggregate {
    rows: u64,
    sum_key: i64,
    max_category: i64,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(database) = args.next() else {
        eprintln!("usage: inillucent-rawcursor <database> [rounds]");
        std::process::exit(2);
    };
    let rounds: u32 = args.next().and_then(|text| text.parse().ok()).unwrap_or(40);
    match run(&PathBuf::from(database), rounds) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("failed: {} :: {error:?}", error.message());
            std::process::exit(1);
        }
    }
}

/// Opens the database, finds the table, and times the bare scan.
fn run(path: &Path, rounds: u32) -> DbResult<()> {
    let vfs: Arc<dyn Vfs> = Arc::new(OsVfs::new());
    let db = DbPath::new(path.to_path_buf());
    // Read-only: nothing here writes, and a writable open would attach a
    // journal whose cost has nothing to do with what is being measured.
    let options = DatabaseOptions {
        writable: false,
        ..DatabaseOptions::default()
    };
    let mut pager = open_database(vfs, &db, options)?;
    let limits = Limits::default();
    // One read transaction held for the whole run. A scan cannot read a page
    // outside one, and taking a fresh one per round would time the shared lock
    // rather than the walk.
    pager.begin_read()?;

    let table = find_root(&mut pager, &limits, "table", "main_table")?
        .ok_or_else(|| inillucent_base::error::misuse("no main_table in this database"))?;
    let covering = find_root(&mut pager, &limits, "index", "main_category")?;
    println!(
        "page size {:?}, {} pages; main_table root {}, main_category root {:?}",
        pager.page_size(),
        pager.page_count(),
        table.get(),
        covering.map(|page| page.get())
    );

    // The table walk is the one the aggregate is defined over. The covering
    // index walk is what SQLite's planner actually chooses for this query, so
    // measuring only the table walk would compare two different amounts of
    // work: the table is 2,779 pages here and the index is 360.
    measure(
        &mut pager,
        &limits,
        "table  main_table",
        table,
        false,
        rounds,
    )?;
    if let Some(index) = covering {
        measure(
            &mut pager,
            &limits,
            "index  main_category",
            index,
            true,
            rounds,
        )?;
    }
    pager.end_read()?;
    Ok(())
}

/// Times one b-tree walk and prints its distribution.
///
/// Best and median are both printed because they answer different questions:
/// the best round is the ceiling this storage layer can reach with a warm
/// cache, and the median is what it does in practice.
fn measure(
    pager: &mut Pager,
    limits: &Limits,
    label: &str,
    root: PageId,
    index: bool,
    rounds: u32,
) -> DbResult<()> {
    let warm = scan(pager, limits, root, index)?;
    let mut times: Vec<u128> = Vec::with_capacity(rounds as usize);
    for _ in 0..rounds {
        let started = Instant::now();
        let out = scan(pager, limits, root, index)?;
        times.push(started.elapsed().as_nanos());
        assert_eq!(out.rows, warm.rows, "the scan must be deterministic");
        assert_eq!(out.sum_key, warm.sum_key, "the scan must be deterministic");
    }
    times.sort_unstable();
    let rows = warm.rows.max(1) as f64;
    let best = times.first().copied().unwrap_or(0);
    let median = times.get(times.len() / 2).copied().unwrap_or(0);
    println!();
    println!(
        "{label}: rows {}, sum(key) {}, max(category) {}",
        warm.rows, warm.sum_key, warm.max_category
    );
    println!("  rounds {rounds}");
    println!(
        "  best   {best:>12} ns   {:>7.2} ns/row",
        best as f64 / rows
    );
    println!(
        "  median {median:>12} ns   {:>7.2} ns/row",
        median as f64 / rows
    );
    Ok(())
}

/// Walks the whole table b-tree, decoding only `key` and `category`.
///
/// This is the hot loop the measurement is about. Everything reusable is
/// hoisted: the payload buffer, the field spans, and the cursor itself.
fn scan(pager: &mut Pager, limits: &Limits, root: PageId, index: bool) -> DbResult<Aggregate> {
    // An index b-tree needs a KeyInfo to compare with, but a full walk never
    // compares: first/next only follow pointers. Plain binary ordering over the
    // three key columns is therefore enough to build the cursor.
    let mut cursor = if index {
        BTreeCursor::index(root, KeyInfo::binary(3))
    } else {
        BTreeCursor::table(root)
    };
    let mut payload: Vec<u8> = Vec::with_capacity(512);
    let mut fields: Vec<FieldSpan> = Vec::with_capacity(8);
    let mut rows = 0u64;
    let mut sum_key = 0i64;
    let mut max_category = i64::MIN;

    let mut more = cursor.first(pager)?;
    while more {
        cursor.payload_into(pager, limits, &mut payload)?;
        let header_len = RecordRef::parse_into(&payload, limits, &mut fields)?;
        let record = RecordRef::with_fields(&payload, &fields, header_len, TextEncoding::Utf8);
        // Column 1 is `key`, column 2 is `category`; both are declared NOT NULL
        // integers, so the serial type is one of the integer widths and the
        // payload can be read as a big-endian two's-complement number without
        // building a `Value`.
        let (key_at, category_at) = if index { (1, 0) } else { (1, 2) };
        sum_key = sum_key.wrapping_add(integer_at(&record, key_at)?);
        let category = integer_at(&record, category_at)?;
        if category > max_category {
            max_category = category;
        }
        rows = rows.saturating_add(1);
        more = cursor.next(pager)?;
    }
    Ok(Aggregate {
        rows,
        sum_key,
        max_category: if rows == 0 { 0 } else { max_category },
    })
}

/// Decodes one integer column straight out of the record's borrowed bytes.
///
/// The serial types 1..6 are big-endian two's-complement integers of 1, 2, 3,
/// 4, 6 and 8 bytes; 8 and 9 are the constants zero and one, which carry no
/// payload. Nothing else is expected in a NOT NULL INTEGER column, and anything
/// else is a corrupt file rather than a case to handle.
fn integer_at(record: &RecordRef<'_>, index: usize) -> DbResult<i64> {
    let serial = record.serial_type(index).map(|kind| kind.0).unwrap_or(0);
    match serial {
        0 | 8 => return Ok(0),
        9 => return Ok(1),
        _ => {}
    }
    let bytes = record.payload(index)?;
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut value = if bytes[0] & 0x80 != 0 { -1i64 } else { 0i64 };
    for byte in bytes {
        value = (value << 8) | i64::from(*byte);
    }
    Ok(value)
}

/// Finds a table's root page by walking `sqlite_schema` with the same cursor.
///
/// Reading the schema through the b-tree rather than through the catalog keeps
/// this binary to the storage layer, which is the layer being measured.
fn find_root(
    pager: &mut Pager,
    limits: &Limits,
    wanted_kind: &str,
    wanted: &str,
) -> DbResult<Option<PageId>> {
    let schema_root = PageId::from_persisted(1)?;
    let mut cursor = BTreeCursor::table(schema_root);
    let mut payload: Vec<u8> = Vec::with_capacity(512);
    let mut fields: Vec<FieldSpan> = Vec::with_capacity(8);
    let mut more = cursor.first(pager)?;
    while more {
        cursor.payload_into(pager, limits, &mut payload)?;
        let header_len = RecordRef::parse_into(&payload, limits, &mut fields)?;
        let record = RecordRef::with_fields(&payload, &fields, header_len, TextEncoding::Utf8);
        let kind = record.payload(0)?;
        let name = record.payload(1)?;
        if kind == wanted_kind.as_bytes() && name == wanted.as_bytes() {
            let root = integer_at(&record, 3)?;
            let root = u32::try_from(root).map_err(|_| {
                inillucent_base::error::corrupt("a root page that is not a page number")
            })?;
            return Ok(Some(PageId::from_persisted(root)?));
        }
        more = cursor.next(pager)?;
    }
    Ok(None)
}
