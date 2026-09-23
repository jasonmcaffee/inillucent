//! Read-only performance baselines for phases 2 and 3.
//!
//! Invariant: this measures, it does not optimise. The ticket is explicit that
//! nothing may be tuned past a measured local hot spot yet, so what this
//! produces is a set of numbers and counters for a later phase to change
//! something against - and nothing here is a comparison against SQLite, nor
//! should any number here be read as one.
//!
//! Six quantities, at three scales, because these are where a read spends its
//! time once the SQL layer exists:
//!
//! - **open and schema load**, which every connection pays once;
//! - **cache hit and cache miss**, which is the difference between a page that
//!   is resident and one that is not, and therefore the whole point of having
//!   a cache;
//! - **a rowid point read**, the cheapest useful query;
//! - **an index point read**, the same thing through a key comparison;
//! - **a range scan**, which is what an index range and an `ORDER BY` become;
//! - **record projection**, reading one column out of a wide row against
//!   reading all of them - the number that says whether the lazy decode is
//!   actually lazy.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-readperf`

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_compat::report::json_string;
use inillucent_compat::{platform_name, workspace_root};
use inillucent_storage::cursor::{BTreeCursor, SeekBias};
use inillucent_storage::pager::{Pager, PagerOptions};
use inillucent_storage::schema;
use inillucent_value::record::{KeyInfo, RecordRef};
use inillucent_value::Value;
use inillucent_vfs::{DbPath, OsVfs};

/// The three scales measured, as row counts.
const SCALES: [(&str, u32); 3] = [("small", 1_000), ("medium", 50_000), ("large", 500_000)];

/// The page size every scale uses, so the scales differ only in size.
const PAGE_SIZE: u32 = 4096;

/// One recorded measurement.
#[derive(Clone, Debug)]
struct Measurement {
    scale: String,
    rows: u32,
    operation: String,
    iterations: u64,
    nanos_per_operation: f64,
    pages_read: u64,
    cache_hits: u64,
}

/// Runs the measurements and writes the artifact.
fn main() -> ExitCode {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    // **Pinned before anything is timed, and the mask printed (task-2085).**
    // Unpinned, a hybrid processor can run this program and the arm it compares
    // against on different core classes, and nothing else in the output says so.
    if let Err(reason) = inillucent_compat::affinity::pin_from_arguments(&mut arguments) {
        eprintln!("{reason}");
        return ExitCode::from(2);
    }
    let out = flag(&arguments, "--out")
        .unwrap_or_else(|| workspace_root().join("_agent_output/read-only-baselines"));
    match run(&out) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}

/// Finds the pinned SQLite shell, which builds the measurement databases.
///
/// The databases are written by SQLite rather than by inillucent on purpose: a
/// baseline measured against a file inillucent laid out would be measuring
/// inillucent's own layout choices, and there are not any yet.
fn pinned_shell(root: &Path) -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_SHELL") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
    }
    let directory = root.join(".sqlite-ref/3.53.4/shell");
    for name in ["sqlite3.exe", "sqlite3"] {
        let path = directory.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!(
        "the pinned SQLite shell is not at {}; run tools/sqlite-reference.ps1 or .sh",
        directory.display()
    ))
}

/// Builds every scale and measures it.
fn run(out: &Path) -> Result<String, String> {
    let root = workspace_root();
    let shell = pinned_shell(&root)?;
    let databases = out.join("db");
    std::fs::create_dir_all(&databases).map_err(|error| error.to_string())?;

    let mut measurements: Vec<Measurement> = Vec::new();
    for (scale, rows) in SCALES {
        let path = databases.join(format!("{scale}.db"));
        if !path.is_file() {
            build_database(&shell, &path, rows)?;
        }
        measurements.extend(measure(&path, scale, rows)?);
    }

    std::fs::create_dir_all(out).map_err(|error| error.to_string())?;
    std::fs::write(
        out.join("read-only-baselines.json"),
        render_json(&measurements),
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(
        out.join("read-only-baselines.md"),
        render_markdown(&measurements),
    )
    .map_err(|error| error.to_string())?;
    Ok(format!(
        "{} measurements written to {}",
        measurements.len(),
        out.display()
    ))
}

/// Writes one measurement database with the pinned shell.
///
/// The table is deliberately wide, because the projection measurement is about
/// what it costs to skip columns, and a three-column table cannot show that.
fn build_database(shell: &Path, path: &Path, rows: u32) -> Result<(), String> {
    let script = format!(
        "PRAGMA page_size = {PAGE_SIZE};\n\
         PRAGMA journal_mode = delete;\n\
         BEGIN;\n\
         CREATE TABLE wide (\n\
           id INTEGER PRIMARY KEY, label TEXT, n INTEGER, r REAL,\n\
           c4 TEXT, c5 TEXT, c6 TEXT, c7 TEXT, c8 TEXT, c9 TEXT,\n\
           c10 TEXT, c11 TEXT, c12 TEXT, c13 TEXT, c14 TEXT, tail TEXT);\n\
         WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM seq WHERE i < {rows})\n\
           INSERT INTO wide SELECT i, 'row-' || printf('%09d', i), i * 7, i / 3.0,\n\
             'c4-' || i, 'c5-' || i, 'c6-' || i, 'c7-' || i, 'c8-' || i, 'c9-' || i,\n\
             'c10-' || i, 'c11-' || i, 'c12-' || i, 'c13-' || i, 'c14-' || i,\n\
             'tail-' || printf('%09d', i)\n\
           FROM seq;\n\
         CREATE INDEX wide_by_label ON wide (label);\n\
         COMMIT;\n\
         .quit\n"
    );
    let script_path = path.with_extension("sql");
    std::fs::write(&script_path, script.as_bytes()).map_err(|error| error.to_string())?;
    let output = Command::new(shell)
        .arg("-batch")
        .arg("-init")
        .arg(&script_path)
        .arg(path)
        .arg(".quit")
        .output()
        .map_err(|error| format!("could not run {}: {error}", shell.display()))?;
    let noise = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if noise.to_ascii_lowercase().contains("error") {
        return Err(format!("building {}: {noise}", path.display()));
    }
    let _ = std::fs::remove_file(&script_path);
    Ok(())
}

/// Times a body, returning nanoseconds per operation.
fn time<F: FnMut(u64)>(iterations: u64, mut body: F) -> f64 {
    let start = Instant::now();
    for index in 0..iterations {
        body(index);
    }
    let elapsed = start.elapsed();
    elapsed.as_nanos() as f64 / iterations.max(1) as f64
}

/// Measures one database at one scale.
fn measure(path: &Path, scale: &str, rows: u32) -> Result<Vec<Measurement>, String> {
    let vfs = OsVfs::new();
    let db = DbPath::new(path);
    let limits = Limits::default();
    let mut measurements = Vec::new();

    // Open and header decode, with no read transaction and a cold cache.
    let iterations = 200u64;
    let nanos = time(iterations, |_| {
        let mut pager =
            Pager::open_read_only(&vfs, &db, PagerOptions::default()).expect("the database opens");
        let _ = pager.header().page_size;
        let _ = pager.close();
    });
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "open".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: 0,
        cache_hits: 0,
    });

    // Schema load, from a fresh cache each time.
    let iterations = 100u64;
    let mut schema_pages = 0u64;
    let nanos = time(iterations, |_| {
        let mut pager =
            Pager::open_read_only(&vfs, &db, PagerOptions::default()).expect("the database opens");
        pager.begin_read().expect("a read starts");
        let objects = schema::load_schema(&mut pager).expect("the schema loads");
        assert!(!objects.is_empty());
        schema_pages = pager.counters().page_reads;
    });
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "schema-load".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: schema_pages,
        cache_hits: 0,
    });

    // One long-lived pager for the rest, so the cache is warm where it should
    // be and cold where it should not.
    let mut pager =
        Pager::open_read_only(&vfs, &db, PagerOptions::default()).map_err(|e| e.to_string())?;
    pager.begin_read().map_err(|e| e.to_string())?;
    let table = schema::find_object(&mut pager, "wide")
        .map_err(|e| e.to_string())?
        .ok_or("the wide table is missing")?;
    let table_root = table.root_page.ok_or("the wide table has no root")?;
    let index = schema::find_object(&mut pager, "wide_by_label")
        .map_err(|e| e.to_string())?
        .ok_or("the index is missing")?;
    let index_root = index.root_page.ok_or("the index has no root")?;

    // Cache hit: the same page, over and over, already resident.
    let iterations = 200_000u64;
    let before = pager.cache_counters();
    let warm = PageId::from_persisted(1).map_err(|e| e.to_string())?;
    let _ = pager.get_page(warm).map_err(|e| e.to_string())?;
    let nanos = time(iterations, |_| {
        let pin = pager.get_page(warm).expect("a resident page reads");
        std::hint::black_box(pin.bytes().first());
    });
    let after = pager.cache_counters();
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "page-read-cache-hit".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: 0,
        cache_hits: after.hits.saturating_sub(before.hits),
    });

    // Cache miss: a fresh pager per read, so the page comes off the disk.
    let iterations = 2_000u64;
    let page_count = pager.pages_in_file();
    let nanos = time(iterations, |index| {
        let mut cold =
            Pager::open_read_only(&vfs, &db, PagerOptions::default()).expect("the database opens");
        cold.begin_read().expect("a read starts");
        let number = (index as u32 % page_count.max(1)).saturating_add(1);
        let page = PageId::from_persisted(number).expect("a page number");
        let pin = cold.get_page(page).expect("a page reads");
        std::hint::black_box(pin.bytes().first());
    });
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "page-read-cache-miss".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: iterations,
        cache_hits: 0,
    });

    // A rowid point read: seek, read the payload, decode the record.
    let iterations = 20_000u64;
    let before = pager.counters();
    let nanos = time(iterations, |index| {
        let rowid = ((index % u64::from(rows)) as i64).saturating_add(1);
        let mut cursor = BTreeCursor::table(table_root);
        let found = cursor
            .seek_rowid(&mut pager, rowid, SeekBias::AtOrAfter)
            .expect("the seek runs");
        assert!(found, "rowid {rowid} is missing");
        let payload = cursor
            .payload(&mut pager, &limits)
            .expect("the payload reads");
        let record = RecordRef::parse_with_limits(&payload, pager.text_encoding(), &limits)
            .expect("a record");
        std::hint::black_box(record.value(1).expect("a field"));
        cursor.reset();
    });
    let after = pager.counters();
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "point-read-by-rowid".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: after.page_reads.saturating_sub(before.page_reads),
        cache_hits: after.cache_hits.saturating_sub(before.cache_hits),
    });

    // An index point read: the same lookup through a key comparison.
    let iterations = 20_000u64;
    let before = pager.counters();
    let key = KeyInfo::binary(1);
    let nanos = time(iterations, |index| {
        let number = (index % u64::from(rows)).saturating_add(1);
        let label = format!("row-{number:09}");
        let probe = vec![Value::owned_text(label.as_bytes()).expect("a probe")];
        let mut cursor = BTreeCursor::index(index_root, key.clone());
        let found = cursor
            .seek_index(&mut pager, &probe, SeekBias::AtOrAfter)
            .expect("the seek runs");
        assert!(found, "{label} is missing from the index");
        std::hint::black_box(
            cursor
                .payload(&mut pager, &limits)
                .expect("the entry reads"),
        );
        cursor.reset();
    });
    let after = pager.counters();
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "point-read-by-index".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: after.page_reads.saturating_sub(before.page_reads),
        cache_hits: after.cache_hits.saturating_sub(before.cache_hits),
    });

    // A range scan of a hundred rows, which is what an index range becomes.
    let iterations = 2_000u64;
    let before = pager.counters();
    let nanos = time(iterations, |index| {
        let start = ((index % u64::from(rows.saturating_sub(100).max(1))) as i64).saturating_add(1);
        let mut cursor = BTreeCursor::table(table_root);
        cursor
            .seek_rowid(&mut pager, start, SeekBias::AtOrAfter)
            .expect("the seek runs");
        let mut seen = 0;
        while cursor.is_positioned() && seen < 100 {
            std::hint::black_box(cursor.rowid().expect("a rowid"));
            seen += 1;
            if !cursor.next(&mut pager).expect("the scan advances") {
                break;
            }
        }
        cursor.reset();
    });
    let after = pager.counters();
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "range-scan-100-rows".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: after.page_reads.saturating_sub(before.page_reads),
        cache_hits: after.cache_hits.saturating_sub(before.cache_hits),
    });

    // Projection: one column out of sixteen, against all sixteen. The gap is
    // the whole argument for decoding lazily.
    let mut cursor = BTreeCursor::table(table_root);
    cursor.first(&mut pager).map_err(|e| e.to_string())?;
    let payload = cursor
        .payload(&mut pager, &limits)
        .map_err(|e| e.to_string())?;
    cursor.reset();
    let encoding = pager.text_encoding();

    let iterations = 200_000u64;
    let nanos = time(iterations, |_| {
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits).expect("a record");
        std::hint::black_box(record.value(15).expect("the last field"));
    });
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "project-one-of-sixteen-columns".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: 0,
        cache_hits: 0,
    });

    let nanos = time(iterations, |_| {
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits).expect("a record");
        std::hint::black_box(record.values().expect("every field"));
    });
    measurements.push(Measurement {
        scale: scale.to_string(),
        rows,
        operation: "project-all-sixteen-columns".to_string(),
        iterations,
        nanos_per_operation: nanos,
        pages_read: 0,
        cache_hits: 0,
    });

    Ok(measurements)
}

/// Renders the measurements as JSON.
fn render_json(measurements: &[Measurement]) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"platform\": {},\n",
        json_string(&platform_name())
    ));
    out.push_str("  \"captured_by\": \"readperf\",\n");
    out.push_str("  \"phase\": \"phases 2-3 read-only baselines\",\n");
    out.push_str(&format!("  \"page_size\": {PAGE_SIZE},\n"));
    out.push_str("  \"measurements\": [\n");
    for (index, measurement) in measurements.iter().enumerate() {
        out.push_str("    {");
        out.push_str(&format!("\"scale\": {}, ", json_string(&measurement.scale)));
        out.push_str(&format!("\"rows\": {}, ", measurement.rows));
        out.push_str(&format!(
            "\"operation\": {}, ",
            json_string(&measurement.operation)
        ));
        out.push_str(&format!("\"iterations\": {}, ", measurement.iterations));
        out.push_str(&format!(
            "\"nanos_per_operation\": {:.1}, ",
            measurement.nanos_per_operation
        ));
        out.push_str(&format!("\"pages_read\": {}, ", measurement.pages_read));
        out.push_str(&format!("\"cache_hits\": {}", measurement.cache_hits));
        out.push('}');
        if index.saturating_add(1) < measurements.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    out
}

/// Renders the measurements as a table.
fn render_markdown(measurements: &[Measurement]) -> String {
    let mut out = String::new();
    out.push_str("# Read-only baselines, phases 2 and 3\n\n");
    out.push_str(&format!("Platform: `{}`\n\n", platform_name()));
    out.push_str(
        "These are baselines, not results. Nothing has been optimised, and nothing here is a\n\
         comparison against SQLite. They exist so that a later phase which changes a read path\n\
         has a number to change it against.\n\n\
         Every database was written by the pinned SQLite 3.53.4 shell at a 4096-byte page size,\n\
         with a sixteen-column table and one index on a text column.\n\n",
    );
    out.push_str("| Scale | Rows | Operation | Iterations | ns/op | Pages read | Cache hits |\n");
    out.push_str("|---|--:|---|--:|--:|--:|--:|\n");
    for measurement in measurements {
        out.push_str(&format!(
            "| {} | {} | `{}` | {} | {:.1} | {} | {} |\n",
            measurement.scale,
            measurement.rows,
            measurement.operation,
            measurement.iterations,
            measurement.nanos_per_operation,
            measurement.pages_read,
            measurement.cache_hits
        ));
    }
    out.push('\n');
    out
}
