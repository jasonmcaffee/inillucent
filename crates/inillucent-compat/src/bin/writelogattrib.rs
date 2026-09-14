//! Attributes `write.insert.batch`'s log volume and time to a specific cause,
//! rather than to "the write path" in general.
//!
//! Invariant: **this reads the log the engine actually wrote, from disk,
//! after the transaction commits.** `docs/roadmap.md` item 5 says the gate's
//! `write.insert.batch` - 2,000 inserts into `main_table` in one transaction,
//! with two secondary indexes - writes 2,491 KiB of log for about 240 KiB of
//! rows, and names two suspects that were already ruled out: the retrieval
//! engine's delta log (never opened by a plain table) and `LeafRef::locate`'s
//! delta-area decode (fixed, and it did not move the gate ratio). This answers
//! the question that is left: which [`inillucent_wal::record::Body`] kind the
//! bytes are actually in, and which of the three trees - `main_table`,
//! `main_key`, `main_category` - they belong to.
//!
//! Every number here comes from one of two places: [`inillucent_wal::record::Record::decode`]
//! walking the segment file the engine wrote, which is exact because it is the
//! same decoder recovery uses; or [`inillucent_engine::ImportedDatabase::write_stats`],
//! which is the engine's own count of what it did. Nothing here is sampled or
//! estimated.
//!
//! Usage:
//!   inillucent-writelogattrib `<medium sqlite fixture>` [--page-size N] [--frames N] [--iterations N]

use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;
use inillucent_wal::record::{kind, Body, Record};

/// How many inserts one round runs, matching the gate's `write.insert.batch`.
const ITERATIONS: u32 = 2_000;

/// `main_table`'s pre-seeded row count at `--scale medium`, for the same
/// reason the gate's own bind formulas need it: `key` and `category` are bound
/// from `main_table`'s existing row count so they land inside it.
const PRESEEDED_ROWS: u32 = 100_000;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-writelogattrib <medium sqlite fixture> [--page-size N] \
             [--frames N] [--iterations N]"
        );
        return ExitCode::from(2);
    };
    let page_size: usize = flag(&arguments, "--page-size")
        .and_then(|value| value.parse().ok())
        .unwrap_or(8_192);
    let frames: usize = flag(&arguments, "--frames")
        .and_then(|value| value.parse().ok())
        .unwrap_or(4_096);
    let iterations: u32 = flag(&arguments, "--iterations")
        .and_then(|value| value.parse().ok())
        .unwrap_or(ITERATIONS);
    match run(Path::new(fixture), page_size, frames, iterations) {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("writelogattrib: {reason}");
            ExitCode::from(2)
        }
    }
}

/// Returns a flag's value, when it was given.
///
/// @param arguments - the command line
/// @param name - the flag, with its dashes
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|value| value == name)?;
    arguments.get(at.saturating_add(1)).cloned()
}

/// Returns an error's detail, for a message.
///
/// @param error - the error
fn why(error: &inillucent_base::DbError) -> String {
    error.detail().unwrap_or(error.message()).to_string()
}

/// One record kind's totals.
#[derive(Clone, Copy, Debug, Default)]
struct KindTotal {
    records: u64,
    bytes: u64,
}

/// One tree's totals, across every kind that names a tree.
#[derive(Clone, Debug, Default)]
struct TreeTotal {
    name: String,
    records: u64,
    bytes: u64,
}

/// What one batch of inserts cost, in the two currencies that matter.
struct Attribution {
    /// Bytes written to the log, indexed by [`inillucent_wal::record::Body::kind`].
    by_kind: [KindTotal; 14],
    /// Bytes written to the log, by which tree the record names.
    by_tree: Vec<TreeTotal>,
    /// Bytes no record ties to a tree - `Commit`, `Checkpoint`, and an
    /// `AllocPage`/`FreePage` for a page the caller does not have a tree handy
    /// for at the call site (see `crate::paged::Extender` in `inillucent-tree`,
    /// which logs a spill page's allocation without its owning tree).
    untied_bytes: u64,
    untied_records: u64,
    /// Total bytes across the whole range.
    total_bytes: u64,
    total_records: u64,
}

/// Runs the attribution and prints it.
///
/// @param fixture - the sqlite fixture `main_table` was built into
/// @param page_size - the new engine's page size
/// @param frames - the buffer pool size, in pages
/// @param iterations - how many inserts to run in the one timed transaction
fn run(fixture: &Path, page_size: usize, frames: usize, iterations: u32) -> Result<(), String> {
    let scratch = workspace_root().join("_agent_output/task-write-insert-batch-attrib/scratch");
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("could not make a scratch directory: {error}"))?;

    println!("## configuration");
    println!("  fixture     : {}", fixture.display());
    println!("  page size   : {page_size}");
    println!("  frames      : {frames}");
    println!("  iterations  : {iterations}");
    println!();

    let indexed = attribute_one(
        fixture, &scratch, "indexed", page_size, frames, iterations, true,
    )?;
    let plain = attribute_one(
        fixture, &scratch, "noindex", page_size, frames, iterations, false,
    )?;

    print_report("WITH main_key and main_category", &indexed);
    println!();
    print_report("WITHOUT either secondary index", &plain);

    println!();
    println!("## index maintenance, by difference");
    println!(
        "  wall time      : {:>9.2} ms indexed, {:>9.2} ms without, {:>9.2} ms for the two indexes",
        indexed.wall_ms,
        plain.wall_ms,
        indexed.wall_ms - plain.wall_ms
    );
    println!(
        "  log bytes      : {:>9} indexed, {:>9} without, {:>9} for the two indexes",
        indexed.attribution.total_bytes,
        plain.attribution.total_bytes,
        indexed
            .attribution
            .total_bytes
            .saturating_sub(plain.attribution.total_bytes)
    );
    println!(
        "  compactions    : {:>9} indexed, {:>9} without, {:>9} for the two indexes",
        indexed.after.compactions,
        plain.after.compactions,
        indexed
            .after
            .compactions
            .saturating_sub(plain.after.compactions)
    );
    println!(
        "  splits         : {:>9} indexed, {:>9} without, {:>9} for the two indexes",
        indexed.after.splits,
        plain.after.splits,
        indexed.after.splits.saturating_sub(plain.after.splits)
    );
    Ok(())
}

/// What one round of the batch cost, gathered before the log is re-read.
struct Round {
    wall_ms: f64,
    find_ms: f64,
    apply_ms: f64,
    after: inillucent_tree::write::WriteStats,
    attribution: Attribution,
}

/// Imports a fixture, runs the insert batch once, and attributes its log.
///
/// @param fixture - the sqlite fixture to import
/// @param scratch - where the working copy and the imported database go
/// @param tag - names this round's files, so the two rounds do not collide
/// @param page_size - the new engine's page size
/// @param frames - the buffer pool size, in pages
/// @param iterations - how many inserts to run
/// @param with_indexes - whether `main_key` and `main_category` are kept
#[allow(clippy::too_many_arguments)]
fn attribute_one(
    fixture: &Path,
    scratch: &Path,
    tag: &str,
    page_size: usize,
    frames: usize,
    iterations: u32,
    with_indexes: bool,
) -> Result<Round, String> {
    let copy = scratch.join(format!("{tag}.db"));
    let _ = std::fs::remove_file(&copy);
    std::fs::copy(fixture, &copy)
        .map_err(|error| format!("could not copy the fixture: {error}"))?;
    if !with_indexes {
        drop_secondary_indexes(&copy)?;
    }

    let mut database = ImportedDatabase::import_with(copy, page_size, frames)
        .map_err(|error| format!("import failed: {}", why(&error)))?;
    let roots = tree_names(&mut database)?;
    // **Not the same number as the "by tree" table below, and printed so a
    // reader does not assume it is.** `sqlite_master.rootpage` is the on-disk
    // page number; a log record's `tree` field is the catalog's own internal
    // root handle, a small integer assigned at import that has no public
    // method mapping it back to a name. The "by tree" table is keyed by that
    // handle, this line by the page number, and the correspondence between
    // them has to be read off which totals move when the indexes are dropped
    // (`main_table`'s is the one that does not change).
    println!("  [{tag}] name -> sqlite_master.rootpage (NOT the log's tree id): {roots:?}");

    let sql = "INSERT INTO main_table(id, key, category, label, payload) \
               VALUES (?1, ?2, ?3, ?4, ?5)";
    let statement = database
        .prepare_statement(sql)
        .map_err(|error| format!("prepare failed: {}", why(&error)))?;

    let lsn_before = database.wal().next_lsn();
    let before = database.write_stats();
    database.begin_batch();
    let mut find_nanos = 0u128;
    let mut apply_nanos = 0u128;
    let started = Instant::now();
    for iteration in 0..iterations {
        let params = Params::from_values(vec![
            OwnedDatum::Int((PRESEEDED_ROWS as i64) + 1 + i64::from(iteration)),
            OwnedDatum::Int(
                (i64::from(iteration)
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(12_345))
                    & 0x7fff_ffff,
            ),
            OwnedDatum::Int(
                (i64::from(iteration)
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(12_345))
                    & 0x7fff_ffff,
            ),
            OwnedDatum::Text(
                format!("row {iteration} lorem ipsum dolor sit amet consectetur").into_bytes(),
            ),
            OwnedDatum::Blob(
                (0..64u64)
                    .map(|j| ((u64::from(iteration) + j) & 0xff) as u8)
                    .collect(),
            ),
        ]);
        let (find, apply) = database
            .execute_timed(&statement, &params)
            .map_err(|error| format!("insert {iteration} failed: {}", why(&error)))?;
        find_nanos = find_nanos.saturating_add(find);
        apply_nanos = apply_nanos.saturating_add(apply);
    }
    database
        .commit_batch()
        .map_err(|error| format!("commit failed: {}", why(&error)))?;
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    database
        .wal()
        .sync()
        .map_err(|error| format!("sync failed: {}", why(&error)))?;
    let lsn_after = database.wal().next_lsn();
    let after = database.write_stats();

    let attribution = attribute_log(&database, lsn_before, lsn_after, &roots)?;

    Ok(Round {
        wall_ms,
        find_ms: find_nanos as f64 / 1e6,
        apply_ms: apply_nanos as f64 / 1e6,
        after: subtract(after, before),
        attribution,
    })
}

/// Returns `after` minus `before`, saturating rather than wrapping.
///
/// @param after - the counters at the end
/// @param before - the counters at the start
fn subtract(
    after: inillucent_tree::write::WriteStats,
    before: inillucent_tree::write::WriteStats,
) -> inillucent_tree::write::WriteStats {
    inillucent_tree::write::WriteStats {
        inserted: after.inserted.saturating_sub(before.inserted),
        deleted: after.deleted.saturating_sub(before.deleted),
        updated_in_place: after
            .updated_in_place
            .saturating_sub(before.updated_in_place),
        compactions: after.compactions.saturating_sub(before.compactions),
        splits: after.splits.saturating_sub(before.splits),
        merges: after.merges.saturating_sub(before.merges),
    }
}

/// Drops `main_key` and `main_category` from a copied fixture, with the pinned
/// shell, so the "without indexes" round writes to the same 100,000 pre-seeded
/// rows and differs from the indexed round in nothing else.
///
/// @param database - the sqlite file to modify in place
fn drop_secondary_indexes(database: &Path) -> Result<(), String> {
    let shell = workspace_root().join(".sqlite-ref/3.53.4/shell/sqlite3.exe");
    let shell = if shell.is_file() {
        shell
    } else {
        workspace_root().join(".sqlite-ref/3.53.4/shell/sqlite3")
    };
    if !shell.is_file() {
        return Err(format!(
            "the pinned sqlite3 shell is not built at {}; run tools/sqlite-reference.ps1 \
             (or .sh) first",
            shell.display()
        ));
    }
    let output = Command::new(&shell)
        .arg(database)
        .arg("DROP INDEX main_key; DROP INDEX main_category;")
        .output()
        .map_err(|error| format!("could not run the pinned shell: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "dropping the secondary indexes failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Returns every tree root this database's catalog names, table or index.
///
/// `sqlite_schema` is the engine's own catalog view, joined like any other
/// table - it is what lets a root page number in a log record be reported as
/// `main_key` rather than as the number nobody but this file's own writer
/// remembers.
///
/// @param database - the imported database
fn tree_names(database: &mut ImportedDatabase) -> Result<Vec<(u64, String)>, String> {
    let outcome = database
        .execute_any(
            "SELECT name, rootpage FROM sqlite_master WHERE type IN ('table', 'index')",
            &Params::new(),
        )
        .map_err(|error| format!("could not read sqlite_master: {}", why(&error)))?;
    let mut roots = Vec::new();
    for row in outcome.rows {
        let mut name = None;
        let mut root = None;
        for (index, value) in row.into_iter().enumerate() {
            match (index, value) {
                (0, OwnedDatum::Text(bytes)) => name = String::from_utf8(bytes).ok(),
                (1, OwnedDatum::Int(number)) if number >= 0 => root = Some(number as u64),
                _ => {}
            }
        }
        if let (Some(name), Some(root)) = (name, root) {
            roots.push((root, name));
        }
    }
    Ok(roots)
}

/// Returns the human name for a record kind byte.
///
/// @param byte - `Body::kind()`'s return
fn kind_name(byte: u8) -> &'static str {
    match byte {
        kind::INSERT_ROW => "InsertRow",
        kind::DELETE_ROW => "DeleteRow",
        kind::UPDATE_IN_PLACE => "UpdateInPlace",
        kind::COMPACT_LEAF => "CompactLeaf",
        kind::SPLIT_LEAF => "Structural(Split)",
        kind::MERGE_LEAF => "Structural(Merge)",
        kind::WRITE_PAGE => "WritePage",
        kind::ALLOC_PAGE => "AllocPage",
        kind::FREE_PAGE => "FreePage",
        kind::COMMIT => "Commit",
        kind::ABORT => "Abort",
        kind::CHECKPOINT => "Checkpoint",
        kind::CATALOG_CHANGE => "CatalogChange",
        _ => "unknown",
    }
}

/// Returns the tree a record body names, when it names one.
///
/// @param body - the decoded record body
fn tree_of(body: &Body<'_>) -> Option<u64> {
    match *body {
        Body::InsertRow { tree, .. }
        | Body::DeleteRow { tree, .. }
        | Body::UpdateInPlace { tree, .. }
        | Body::CompactLeaf { tree, .. }
        | Body::Structural { tree, .. } => Some(tree),
        _ => None,
    }
}

/// Walks every segment the log holds and tallies bytes and counts, by kind and
/// by tree, over records whose LSN falls in `[from, to)`.
///
/// Reads the log the way recovery does - `Record::decode`, one record at a
/// time, off the actual segment file - rather than trusting a running total,
/// because the question this binary answers is "what did the engine write",
/// and the segment file is the only witness that cannot be wrong about that.
///
/// @param database - the imported database, for its log's segment paths
/// @param from - the first LSN this transaction wrote, inclusive
/// @param to - the LSN past the last byte this transaction wrote
/// @param roots - every tree root the catalog names, for labelling
fn attribute_log(
    database: &ImportedDatabase,
    from: u64,
    to: u64,
    roots: &[(u64, String)],
) -> Result<Attribution, String> {
    let wal = database.wal();
    let mut by_kind = [KindTotal::default(); 14];
    let mut by_tree_bytes = std::collections::BTreeMap::<u64, (u64, u64)>::new();
    let mut untied_bytes = 0u64;
    let mut untied_records = 0u64;
    let mut total_bytes = 0u64;
    let mut total_records = 0u64;

    for sequence in 1..=wal.sequence() {
        let path = wal.segment_path(sequence);
        let bytes = std::fs::read(path.as_path())
            .map_err(|error| format!("could not read segment {sequence}: {error}"))?;
        let mut at = inillucent_wal::segment::HEADER_BYTES;
        while at < bytes.len() {
            let Some(slice) = bytes.get(at..) else {
                break;
            };
            let Some(record) = Record::decode(slice)
                .map_err(|error| format!("segment {sequence} at {at}: {}", why(&error)))?
            else {
                break;
            };
            if record.lsn >= from && record.lsn < to {
                let kind_byte = record.body.kind();
                if let Some(slot) = by_kind.get_mut(kind_byte as usize) {
                    slot.records = slot.records.saturating_add(1);
                    slot.bytes = slot.bytes.saturating_add(record.length as u64);
                }
                total_records = total_records.saturating_add(1);
                total_bytes = total_bytes.saturating_add(record.length as u64);
                match tree_of(&record.body) {
                    Some(tree) => {
                        let entry = by_tree_bytes.entry(tree).or_insert((0, 0));
                        entry.0 = entry.0.saturating_add(1);
                        entry.1 = entry.1.saturating_add(record.length as u64);
                    }
                    None => {
                        untied_records = untied_records.saturating_add(1);
                        untied_bytes = untied_bytes.saturating_add(record.length as u64);
                    }
                }
            }
            at = at.saturating_add(record.length);
        }
    }

    let mut by_tree: Vec<TreeTotal> = by_tree_bytes
        .into_iter()
        .map(|(root, (records, bytes))| TreeTotal {
            name: roots
                .iter()
                .find(|(candidate, _)| *candidate == root)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| format!("tree {root}")),
            records,
            bytes,
        })
        .collect();
    by_tree.sort_by_key(|row| std::cmp::Reverse(row.bytes));

    Ok(Attribution {
        by_kind,
        by_tree,
        untied_bytes,
        untied_records,
        total_bytes,
        total_records,
    })
}

/// Prints one round's report.
///
/// @param label - what this round measured
/// @param round - the timings, the stats delta, and the log attribution
fn print_report(label: &str, round: &Round) {
    println!("## {label}");
    println!(
        "  wall {:>8.2} ms   find {:>8.2} ms   apply {:>8.2} ms   \
         inserted {:>6}   compactions {:>5}   splits {:>4}   merges {:>4}",
        round.wall_ms,
        round.find_ms,
        round.apply_ms,
        round.after.inserted,
        round.after.compactions,
        round.after.splits,
        round.after.merges,
    );
    println!(
        "  log: {} bytes over {} records ({:.1} KiB)",
        round.attribution.total_bytes,
        round.attribution.total_records,
        round.attribution.total_bytes as f64 / 1024.0
    );
    println!("  by record kind:");
    for (index, total) in round.attribution.by_kind.iter().enumerate() {
        if total.records == 0 {
            continue;
        }
        println!(
            "    {:<20} {:>7} records {:>10} bytes  ({:>6.1} KiB, avg {:>6} B/record)",
            kind_name(index as u8),
            total.records,
            total.bytes,
            total.bytes as f64 / 1024.0,
            total.bytes.checked_div(total.records).unwrap_or(0)
        );
    }
    println!("  by tree:");
    for tree in &round.attribution.by_tree {
        println!(
            "    {:<16} {:>7} records {:>10} bytes  ({:>6.1} KiB)",
            tree.name,
            tree.records,
            tree.bytes,
            tree.bytes as f64 / 1024.0
        );
    }
    if round.attribution.untied_records > 0 {
        println!(
            "    {:<16} {:>7} records {:>10} bytes  ({:>6.1} KiB)",
            "(no tree)",
            round.attribution.untied_records,
            round.attribution.untied_bytes,
            round.attribution.untied_bytes as f64 / 1024.0
        );
    }
}
