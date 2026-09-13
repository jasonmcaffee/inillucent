//! How the cost of a write grows with the size of the transaction it is in.
//!
//! Invariant: this measures, it does not optimise, and it is not a comparison
//! against SQLite. It exists because the scorecard reports the same statement -
//! `UPDATE side_table SET note = ?2 WHERE id = ?1` - at ratios that move with
//! how many of its siblings share its transaction. A statement whose ratio
//! depends on how many of its siblings share its transaction is not a slow
//! statement; it is a cost that grows with the transaction, and the way to
//! tell which is to vary only that.
//!
//! A flat nanoseconds-per-write column means the cost is per write. A column
//! that climbs with the batch size means it is quadratic in the batch, and the
//! slope says how much of it is.
//!
//! **The per-opcode and per-stage breakdowns this file used to print are
//! gone.** They read `inillucent_base::probe::OPCODE_*`/`STAGE_*`, which that
//! module's own doc comment ties to the old virtual machine's `opcode-probe`
//! feature - and the new engine has no virtual machine to attribute an
//! allocation to an opcode of. The stage tables have no writer left in the
//! workspace at all (`record_stage`/`record_stage_allocating` have no caller
//! outside their own definitions), so both sections would have printed
//! nothing on this engine regardless of the deletion.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-txnprofile`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use inillucent_engine::connect::{Connection, Database};

/// How many rows the table holds, which every batch size updates within.
const ROWS: u32 = 20_000;

fn main() -> ExitCode {
    let root = PathBuf::from("_agent_output/txnprofile");
    match run(&root) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("txnprofile: {failure}");
            ExitCode::FAILURE
        }
    }
}

/// Measures one batch size at a time, largest last.
fn run(root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|failure| failure.to_string())?;
    println!(
        "{:>10} {:>14} {:>16} {:>14}",
        "batch", "total", "ns/write", "vs batch=50"
    );
    let mut smallest: Option<f64> = None;
    for batch in [50u32, 100, 250, 500, 1_000, 2_000, 4_000, 8_000] {
        let per_write = measure_batch(root, batch)?;
        let reference = *smallest.get_or_insert(per_write);
        println!(
            "{:>10} {:>13.2}ms {:>16.1} {:>13.2}x",
            batch,
            per_write * f64::from(batch) / 1_000_000.0,
            per_write,
            per_write / reference.max(1.0),
        );
    }
    println!();
    measure_fts(root)?;
    println!();
    let per_insert = measure_inserts(root)?;
    println!("insert into a two-index table: {per_insert:.1} ns each");
    Ok(())
}

/// Measures the cost of an FTS5 insert as the index grows.
///
/// The module keeps one doclist per term, so a document's terms each have their
/// whole posting list rewritten when it is inserted. If that is what is
/// happening, the cost of one insert rises with the number of documents already
/// there and the total is quadratic - which is what a flat column here would
/// disprove and a rising one confirms. The documents share a vocabulary on
/// purpose: a corpus where every term is unique would grow no doclist.
fn measure_fts(root: &Path) -> Result<(), String> {
    println!(
        "{:>10} {:>14} {:>16} {:>14}",
        "documents", "total", "ns/insert", "vs first"
    );
    let mut first: Option<f64> = None;
    for count in [100u32, 200, 400, 800, 1_600] {
        let path = root.join(format!("fts-{count}.db"));
        if path.exists() {
            std::fs::remove_file(&path).map_err(|failure| failure.to_string())?;
        }
        let database = Database::open(&path).map_err(|failure| failure.to_string())?;
        let connection = database.connect();
        connection
            .execute_batch("CREATE VIRTUAL TABLE documents USING fts5(title, body)")
            .map_err(|failure| failure.to_string())?;
        let mut insert = connection
            .prepare("INSERT INTO documents(title, body) VALUES (?1, ?2)")
            .map_err(|failure| failure.to_string())?;
        connection
            .execute_batch("BEGIN")
            .map_err(|failure| failure.to_string())?;
        let started = Instant::now();
        for index in 0..count {
            insert.reset();
            insert
                .bind_text(1, "lorem ipsum dolor sit amet")
                .map_err(|failure| failure.to_string())?;
            let body = format!(
                "lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                 eiusmod tempor incididunt ut labore document number {index}"
            );
            insert
                .bind_text(2, &body)
                .map_err(|failure| failure.to_string())?;
            while insert.step().map_err(|failure| failure.to_string())? {}
        }
        let elapsed = started.elapsed().as_nanos() as f64;
        connection
            .execute_batch("COMMIT")
            .map_err(|failure| failure.to_string())?;
        let per = elapsed / f64::from(count.max(1));
        let reference = *first.get_or_insert(per);
        println!(
            "{:>10} {:>13.2}ms {:>16.1} {:>13.2}x",
            count,
            elapsed / 1_000_000.0,
            per,
            per / reference.max(1.0)
        );
    }
    Ok(())
}

/// Measures inserts into the scorecard's own two-index table, in one
/// transaction, which is the `write.insert.batch` shape.
fn measure_inserts(root: &Path) -> Result<f64, String> {
    let path = root.join("insert.db");
    if path.exists() {
        std::fs::remove_file(&path).map_err(|failure| failure.to_string())?;
    }
    let database = Database::open(&path).map_err(|failure| failure.to_string())?;
    let connection = database.connect();
    for statement in [
        "CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL,          category INTEGER NOT NULL, label TEXT NOT NULL, payload BLOB)",
        "CREATE INDEX main_key ON main_table(key)",
        "CREATE INDEX main_category ON main_table(category, key)",
    ] {
        connection
            .execute_batch(statement)
            .map_err(|failure| format!("{statement}: {failure}"))?;
    }
    let mut insert = connection
        .prepare(
            "INSERT INTO main_table(id, key, category, label, payload)              VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(|failure| failure.to_string())?;
    connection
        .execute_batch("BEGIN")
        .map_err(|failure| failure.to_string())?;
    const COUNT: u32 = 20_000;
    let started = Instant::now();
    for index in 0..COUNT {
        insert.reset();
        insert
            .bind_integer(1, i64::from(index) + 1)
            .map_err(|failure| failure.to_string())?;
        insert
            .bind_integer(2, i64::from(index.wrapping_mul(7_919) % COUNT))
            .map_err(|failure| failure.to_string())?;
        insert
            .bind_integer(3, i64::from(index % 32))
            .map_err(|failure| failure.to_string())?;
        insert
            .bind_text(4, "a label of an ordinary length")
            .map_err(|failure| failure.to_string())?;
        insert.bind_null(5).map_err(|failure| failure.to_string())?;
        while insert.step().map_err(|failure| failure.to_string())? {}
    }
    let elapsed = started.elapsed().as_nanos() as f64;
    connection
        .execute_batch("COMMIT")
        .map_err(|failure| failure.to_string())?;
    Ok(elapsed / f64::from(COUNT))
}

/// Runs one transaction of `batch` updates and returns the cost of each.
///
/// A fresh database per batch size, so the measurement is of the transaction
/// and not of whatever the previous one left in the file.
fn measure_batch(root: &Path, batch: u32) -> Result<f64, String> {
    let path = root.join(format!("txn-{batch}.db"));
    if path.exists() {
        std::fs::remove_file(&path).map_err(|failure| failure.to_string())?;
    }
    let database = Database::open(&path).map_err(|failure| failure.to_string())?;
    let connection = database.connect();
    build(&connection)?;

    let mut update = connection
        .prepare("UPDATE side_table SET note = ?2 WHERE id = ?1")
        .map_err(|failure| failure.to_string())?;
    connection
        .execute_batch("BEGIN")
        .map_err(|failure| failure.to_string())?;
    let started = Instant::now();
    for index in 0..batch {
        // Scattered rather than sequential, so the pages a batch touches grow
        // with the batch the way the scorecard's binding does.
        let row = i64::from((index.wrapping_mul(7_919)) % ROWS) + 1;
        update.reset();
        update
            .bind_integer(1, row)
            .map_err(|failure| failure.to_string())?;
        update
            .bind_text(2, "a note of a fairly ordinary length for this table")
            .map_err(|failure| failure.to_string())?;
        while update.step().map_err(|failure| failure.to_string())? {}
    }
    let elapsed = started.elapsed().as_nanos() as f64;
    connection
        .execute_batch("COMMIT")
        .map_err(|failure| failure.to_string())?;
    Ok(elapsed / f64::from(batch.max(1)))
}

/// Builds the table the updates run against.
fn build(connection: &Connection<'_>) -> Result<(), String> {
    for statement in [
        "CREATE TABLE side_table(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, note TEXT)",
        "CREATE INDEX side_owner ON side_table(owner)",
        "CREATE TABLE digits(n INTEGER PRIMARY KEY)",
    ] {
        connection
            .execute_batch(statement)
            .map_err(|failure| format!("{statement}: {failure}"))?;
    }
    connection
        .execute_batch("INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)")
        .map_err(|failure| failure.to_string())?;
    let insert = format!(
        "INSERT INTO side_table(id, owner, note) \
         SELECT seq, seq % 97, 'note-' || seq \
         FROM (SELECT (((d0.n * 10 + d1.n) * 10 + d2.n) * 10 + d3.n) * 10 + d4.n + 1 AS seq \
         FROM digits d0, digits d1, digits d2, digits d3, digits d4) WHERE seq <= {ROWS}"
    );
    connection
        .execute_batch(&insert)
        .map_err(|failure| format!("insert: {failure}"))?;
    Ok(())
}
