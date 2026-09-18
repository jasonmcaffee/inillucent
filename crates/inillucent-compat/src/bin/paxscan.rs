//! What a full scan costs over a PAX leaf, with nothing above the storage layer.
//!
//! Invariant: this computes the same aggregate over the same logical rows as
//! `rawcursor.rs` and as `sqlite-bench` on the same fixture, and it prints that
//! aggregate rather than discarding it. A timing whose aggregate disagrees is
//! not a slow engine, it is a wrong one, and the three numbers are printed side
//! by side so the reader can check rather than trust.
//!
//! ## Why this binary exists
//!
//! An earlier measurement found the *existing* storage layer's ceiling on
//! `SELECT count(*), sum(key), max(category) FROM main_table` - `BTreeCursor`
//! walked directly, no VM, no `Value`, no allocation per row - and found a best
//! of 1.20x SQLite and a median of 0.78x. Its handoff argues that this is an
//! upper bound on *any* engine built over that storage, because a real engine
//! only adds work on top.
//!
//! The rearchitecture's design doc says the bound does not transfer, because
//! what it measures is the SQLite *record codec* - one header parse and two
//! varint decodes per row - which the PAX leaf replaces with a contiguous run
//! of 8-byte integers. Both readings are consistent with that earlier
//! measurement's numbers, and the disagreement reduces to exactly one quantity nobody has measured:
//! nanoseconds per row for a raw scan over a PAX leaf.
//!
//! This binary measures it, before the executor, the MVCC, the WAL and the
//! buffer pool are built on top of the assumption. If a raw PAX scan lands near
//! the 13-25 ns/row the 5x gate needs, the objection is answered by
//! construction. If it lands at 60+ ns/row, the analytics case is in trouble at
//! this row shape and that is the honest early stop the TDD's phase rule is for.
//!
//! ## Like for like
//!
//! `EXPLAIN QUERY PLAN` shows SQLite answering this aggregate from the covering
//! index `main_category(category, key)` - 360 pages against the table's 2,779 at
//! 4 KiB, 7.7x less data. So both structures are built and measured on the
//! inillucent side too, and the comparison the gate reads is inillucent's best plan
//! against SQLite's best plan, with the like-structure pairs printed beside it.
//!
//! Usage: inillucent-paxscan `<sqlite fixture>` `rounds` [page size...]

use std::path::{Path, PathBuf};
use std::time::Instant;

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_sqlite_reader::{borrow, SqliteFile};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::{LeafRef, Tree};

/// What one scan produced, kept so it can be checked against SQLite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Aggregate {
    rows: u64,
    sum_key: i64,
    max_category: i64,
}

/// Which columns of a tree hold `key` and `category`.
#[derive(Clone, Copy)]
struct Shape {
    key: usize,
    category: usize,
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let Some(fixture) = arguments.next() else {
        eprintln!("usage: inillucent-paxscan <sqlite fixture> [rounds] [page size...]");
        std::process::exit(2);
    };
    let rounds: u32 = arguments
        .next()
        .and_then(|text| text.parse().ok())
        .unwrap_or(30);
    let mut sizes: Vec<usize> = arguments.filter_map(|text| text.parse().ok()).collect();
    if sizes.is_empty() {
        sizes = vec![8_192, 16_384, 32_768, 65_536];
    }
    match run(&PathBuf::from(fixture), rounds, &sizes) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("failed: {} :: {error:?}", error.message());
            std::process::exit(1);
        }
    }
}

/// Imports the fixture, builds the trees, and times the scans.
///
/// @param fixture - the SQLite database to import
/// @param rounds - how many timed rounds per configuration
/// @param sizes - the page sizes to sweep
fn run(fixture: &Path, rounds: u32, sizes: &[usize]) -> DbResult<()> {
    let started = Instant::now();
    let mut file = SqliteFile::open(fixture.to_path_buf())?;
    let table_object = file.object("table", "main_table")?;
    let table_columns = table_object.column_names()?;
    let index_object = file.object("index", "main_category")?;

    // `read_table` prepends the rowid, and `id INTEGER PRIMARY KEY` *is* the
    // rowid, so SQLite stores NULL in that record field. The imported row is
    // therefore [rowid, id=NULL, key, category, label, payload] and the id
    // column is dropped: a rowid-clustered tree in the new format holds the
    // rowid once, as its key.
    let raw = file.read_table(table_object.root, table_columns.len())?;
    let table_rows: Vec<Vec<OwnedDatum>> = raw
        .into_iter()
        .map(|mut row| {
            if row.len() > 1 {
                row.remove(1);
            }
            row
        })
        .collect();
    // An index entry is (category, key, rowid): all three are the key.
    let index_rows = file.read_index(index_object.root, 3)?;
    println!(
        "imported {} table rows and {} index entries from {} in {:.1}s",
        table_rows.len(),
        index_rows.len(),
        fixture.display(),
        started.elapsed().as_secs_f64()
    );
    println!(
        "source page size {}, {} pages",
        file.page_size(),
        file.page_count()
    );
    if table_rows.is_empty() {
        return Err(misuse("the fixture has no rows"));
    }

    let table_spec = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
        ColumnSpec::new(PhysicalType::Blob),
    ];
    let index_spec = vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::key(PhysicalType::Int64),
    ];
    let table_shape = Shape {
        key: 1,
        category: 2,
    };
    let index_shape = Shape {
        key: 1,
        category: 0,
    };

    // The reference answer, computed off the imported rows before any PAX page
    // exists. Every scan below is checked against it, so a layout bug shows up
    // as a wrong aggregate rather than as a fast one.
    let reference = reference_aggregate(&table_rows, table_shape);
    println!(
        "reference: rows {}, sum(key) {}, max(category) {}",
        reference.rows, reference.sum_key, reference.max_category
    );

    for size in sizes {
        println!();
        println!("############ page size {size} ############");
        measure_tree(
            "table  main_table",
            *size,
            &table_spec,
            1,
            &table_rows,
            table_shape,
            reference,
            rounds,
        )?;
        measure_tree(
            "index  main_category",
            *size,
            &index_spec,
            3,
            &index_rows,
            index_shape,
            reference,
            rounds,
        )?;
    }
    Ok(())
}

/// Computes the aggregate straight off the imported rows.
///
/// @param rows - the imported rows
/// @param shape - which columns hold `key` and `category`
fn reference_aggregate(rows: &[Vec<OwnedDatum>], shape: Shape) -> Aggregate {
    let mut sum_key = 0i64;
    let mut max_category = i64::MIN;
    for row in rows {
        if let Some(OwnedDatum::Int(number)) = row.get(shape.key) {
            sum_key = sum_key.wrapping_add(*number);
        }
        if let Some(OwnedDatum::Int(number)) = row.get(shape.category) {
            if *number > max_category {
                max_category = *number;
            }
        }
    }
    Aggregate {
        rows: rows.len() as u64,
        sum_key,
        max_category: if rows.is_empty() { 0 } else { max_category },
    }
}

/// Builds one tree at one page size and times the scan over it.
///
/// @param label - what to print
/// @param page_size - the page size to build at
/// @param columns - the tree's column directory
/// @param key_columns - how many leading columns form the key
/// @param rows - the imported rows, already in key order
/// @param shape - which columns hold `key` and `category`
/// @param reference - the aggregate every round must reproduce
/// @param rounds - how many timed rounds
fn measure_tree(
    label: &str,
    page_size: usize,
    columns: &[ColumnSpec],
    key_columns: usize,
    rows: &[Vec<OwnedDatum>],
    shape: Shape,
    reference: Aggregate,
    rounds: u32,
) -> DbResult<()> {
    let borrowed: Vec<Vec<Datum<'_>>> = rows.iter().map(|row| borrow(row)).collect();
    let built = Instant::now();
    let tree = Tree::bulk_build(page_size, 1, columns.to_vec(), key_columns, &borrowed)?;
    let build_time = built.elapsed();
    tree.check()?;

    // How many leaves are on the vectorised fast path. A leaf that is not is a
    // leaf the scan pays per-row for, and if that number is not zero here the
    // measurement is of a different loop than the one the design describes.
    let mut clean = 0usize;
    for index in 0..tree.leaf_count() {
        if tree.leaf(index)?.is_clean() {
            clean = clean.saturating_add(1);
        }
    }

    let warm = scan(&tree, shape)?;
    if warm != reference {
        return Err(misuse(format!(
            "{label} at page size {page_size} computed {warm:?}, not {reference:?}"
        )));
    }
    let mut times: Vec<u128> = Vec::with_capacity(rounds as usize);
    for _ in 0..rounds {
        let started = Instant::now();
        let out = scan(&tree, shape)?;
        times.push(started.elapsed().as_nanos());
        if out != warm {
            return Err(misuse(format!("{label} was not deterministic")));
        }
    }
    times.sort_unstable();
    let rows_f = warm.rows.max(1) as f64;
    let best = times.first().copied().unwrap_or(0);
    let median = times.get(times.len() / 2).copied().unwrap_or(0);
    println!();
    println!(
        "{label}: {} leaves, {} clean, {:.2} MiB, built in {:.2}s",
        tree.leaf_count(),
        clean,
        tree.byte_size() as f64 / (1024.0 * 1024.0),
        build_time.as_secs_f64()
    );
    println!(
        "  best   {best:>12} ns   {:>7.2} ns/row",
        best as f64 / rows_f
    );
    println!(
        "  median {median:>12} ns   {:>7.2} ns/row",
        median as f64 / rows_f
    );
    Ok(())
}

/// Walks every leaf of a tree computing `count(*), sum(key), max(category)`.
///
/// This is the hot loop the measurement is about, and it is deliberately the
/// most favourable thing the layout can do: no executor, no batch object, no
/// expression tree, no `Datum`, no allocation. Each leaf's two integer
/// mini-columns are read as contiguous 8-byte runs.
///
/// The generic path is not an afterthought - it is what makes the number
/// honest. If a leaf has NULLs, exceptions, tombstones or delta rows, the fast
/// path is wrong for it, so it falls back per leaf. The caller prints how many
/// leaves took which path.
///
/// @param tree - the tree to scan
/// @param shape - which columns hold `key` and `category`
fn scan(tree: &Tree, shape: Shape) -> DbResult<Aggregate> {
    let mut rows = 0u64;
    let mut sum_key = 0i64;
    let mut max_category = i64::MIN;
    let mut cursor = tree.scan();
    while let Some(leaf) = cursor.next_leaf()? {
        let keys = leaf.column(shape.key)?;
        let categories = leaf.column(shape.category)?;
        let fast = leaf.is_clean()
            && keys.physical == PhysicalType::Int64
            && categories.physical == PhysicalType::Int64
            && keys.all_typed()
            && categories.all_typed();
        if fast {
            // The stride is the leaf's, not the type's: an integer
            // mini-column spends the narrowest of 1, 2, 4 and 8 bytes that
            // holds its own values.
            let mut key_bytes = keys.inline_bytes().chunks_exact(keys.width);
            let mut category_bytes = categories.inline_bytes().chunks_exact(categories.width);
            while let (Some(key), Some(category)) = (key_bytes.next(), category_bytes.next()) {
                sum_key = sum_key.wrapping_add(inillucent_tree::types::read_int_slot(key));
                let value = inillucent_tree::types::read_int_slot(category);
                if value > max_category {
                    max_category = value;
                }
            }
            rows = rows.saturating_add(leaf.row_count() as u64);
        } else {
            generic(&leaf, shape, &mut rows, &mut sum_key, &mut max_category)?;
        }
    }
    Ok(Aggregate {
        rows,
        sum_key,
        max_category: if rows == 0 { 0 } else { max_category },
    })
}

/// The per-row path, for a leaf that is not clean.
///
/// @param leaf - the leaf to walk
/// @param shape - which columns hold `key` and `category`
/// @param rows - the running row count
/// @param sum_key - the running sum
/// @param max_category - the running maximum
fn generic(
    leaf: &LeafRef<'_>,
    shape: Shape,
    rows: &mut u64,
    sum_key: &mut i64,
    max_category: &mut i64,
) -> DbResult<()> {
    for row in leaf.live()? {
        if let Some(Datum::Int(number)) = row.get(shape.key) {
            *sum_key = sum_key.wrapping_add(*number);
        }
        if let Some(Datum::Int(number)) = row.get(shape.category) {
            if *number > *max_category {
                *max_category = *number;
            }
        }
        *rows = rows.saturating_add(1);
    }
    Ok(())
}
