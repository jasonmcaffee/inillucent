//! What the SQL path costs over asking the retrieval store directly.
//!
//! Invariant: **both arms ask the same store the same question over the same
//! data, in the same process.** The number this exists to produce is the
//! *overhead* of reaching an index through `ORDER BY vector_distance_cos(v, ?)
//! LIMIT k` rather than through the module's own query - the planner match, the
//! probe, the rowid descents, and the exact rescore - so anything that differs
//! between the arms other than the path would be measuring something else.
//!
//! It is deliberately **not** a comparison against `inillucent-core`'s 0.704 ms
//! p50. That figure is on the graded 185k-chunk corpus at 768 dimensions and
//! this is not that corpus; quoting the two side by side would be comparing a
//! measurement to a different measurement's number. What is comparable is the
//! pair below, and the direct arm is the same code the 0.704 ms was taken over.
//!
//! It also reports the planned arm's **recall** against a cosine this binary
//! computes over the same generated vectors, because the index's default mode
//! is `approximate` and a time without the accuracy it bought is half a
//! measurement. `--exact` builds the index `WITH (mode = 'exact')` instead, so
//! the two modes can be compared on one corpus, and `--ef-search N` adds
//! `ef_search = N` to see what a wider graph walk buys.
//!
//! Usage:
//!   inillucent-vectorprobe [--rows N] [--dims N] [--k N] [--probes N] [--exact]
//!                          [--ef-search N]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;

/// How many rows the corpus holds by default.
const DEFAULT_ROWS: usize = 20_000;

/// How many dimensions each vector has by default.
const DEFAULT_DIMS: usize = 256;

/// How many neighbours each probe asks for by default.
const DEFAULT_K: usize = 10;

/// How many probes each arm runs by default.
const DEFAULT_PROBES: usize = 200;

/// Returns one deterministic pseudo-random vector.
///
/// A fixed generator rather than a crate, so the corpus is the same on every
/// machine and a number can be compared with the one before it.
///
/// @param seed - which vector
/// @param dims - how wide it is
fn vector_of(seed: u64, dims: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..dims)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f32 / (1u64 << 53) as f32) - 0.5
        })
        .collect()
}

/// Renders a vector as the blob literal a statement can carry.
///
/// @param values - the vector
fn literal(values: &[f32]) -> String {
    let mut out = String::with_capacity(values.len() * 8 + 3);
    out.push_str("x'");
    for value in values {
        for byte in value.to_bits().to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out.push('\'');
    out
}

/// Returns the median of a list of milliseconds, sorting it in place.
///
/// @param values - the samples
fn middle(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// Returns the value at a percentile of a sorted list.
///
/// @param sorted - the samples, already sorted
/// @param share - the percentile, as a fraction
fn at(sorted: &[f64], share: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * share).round() as usize;
    sorted.get(index).copied().unwrap_or(0.0)
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let rows = flag(&arguments, "--rows").unwrap_or(DEFAULT_ROWS);
    let dims = flag(&arguments, "--dims").unwrap_or(DEFAULT_DIMS);
    let k = flag(&arguments, "--k").unwrap_or(DEFAULT_K);
    let probes = flag(&arguments, "--probes").unwrap_or(DEFAULT_PROBES);
    let mut settings = if arguments.iter().any(|held| held == "--exact") {
        String::from("mode = 'exact'")
    } else {
        String::from("mode = 'approximate'")
    };
    if let Some(ef) = flag(&arguments, "--ef-search") {
        settings.push_str(&format!(", ef_search = {ef}"));
    }
    match run(rows, dims, k, probes, &settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("vectorprobe: {why}");
            ExitCode::FAILURE
        }
    }
}

/// Builds the corpus, then times both ways of asking it the same question.
///
/// @param rows - how many vectors
/// @param dims - how wide each one is
/// @param k - how many neighbours a probe asks for
/// @param probes - how many probes each arm runs
/// @param settings - the index's `WITH ( ... )` settings, which always name a mode
fn run(rows: usize, dims: usize, k: usize, probes: usize, settings: &str) -> Result<(), String> {
    let root = inillucent_compat::workspace_root().join("_agent_output/vectorprobe");
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let path: PathBuf = root.join(format!("{}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut database = ImportedDatabase::create(path, 32_768, 4_096)
        .map_err(|error| format!("the database opens: {error:?}"))?;
    database
        .execute_any(
            &format!("CREATE TABLE embedding (id INTEGER PRIMARY KEY, v VECTOR({dims}))"),
            &Params::new(),
        )
        .map_err(|error| format!("the table is created: {error:?}"))?;

    println!("## the SQL path's overhead over the store's own query");
    println!("  rows  : {rows}");
    println!("  dims  : {dims}");
    println!("  k     : {k}");
    println!("  probes: {probes}");
    println!("  index : WITH ({settings})");

    let loading = Instant::now();
    // In batches, because one statement per row is a transaction per row and
    // this is not measuring commits.
    let mut batch = String::new();
    for index in 0..rows {
        batch.push_str(&format!(
            "INSERT INTO embedding(id, v) VALUES ({index}, {}); ",
            literal(&vector_of(index as u64, dims))
        ));
        if batch.len() > 1_000_000 || index + 1 == rows {
            for statement in batch.split_inclusive(';') {
                if statement.trim().is_empty() {
                    continue;
                }
                database
                    .execute_any(statement.trim(), &Params::new())
                    .map_err(|error| format!("the corpus loads: {error:?}"))?;
            }
            batch.clear();
        }
    }
    println!("  load  : {:.2} s", loading.elapsed().as_secs_f64());

    let building = Instant::now();
    database
        .execute_any(
            &format!("CREATE INDEX ix ON embedding USING inillucent_hnsw (v) WITH ({settings})"),
            &Params::new(),
        )
        .map_err(|error| format!("the index is built: {error:?}"))?;
    println!("  build : {:.2} s", building.elapsed().as_secs_f64());

    let mut planned: Vec<f64> = Vec::with_capacity(probes);
    let mut direct: Vec<f64> = Vec::with_capacity(probes);
    let mut found = 0usize;
    for probe in 0..probes {
        let seed = 1_000_000 + probe as u64;
        let vector = literal(&vector_of(seed, dims));
        let sql =
            format!("SELECT id FROM embedding ORDER BY vector_distance_cos(v, {vector}) LIMIT {k}");
        let started = Instant::now();
        let answer = database
            .execute_any(&sql, &Params::new())
            .map_err(|error| format!("the planned query: {error:?}"))?;
        planned.push(started.elapsed().as_secs_f64() * 1000.0);
        if answer.rows.len() != k {
            return Err(format!(
                "the planned query answered {} rows, not {k}",
                answer.rows.len()
            ));
        }
        found += shared_neighbours(&answer.rows, &nearest(seed, rows, dims, k));
        let sql = format!(
            "SELECT rowid FROM ix WHERE ix MATCH '' AND vector = {vector} AND k = {k} \
             ORDER BY rank"
        );
        let started = Instant::now();
        let answer = database
            .execute_any(&sql, &Params::new())
            .map_err(|error| format!("the direct query: {error:?}"))?;
        direct.push(started.elapsed().as_secs_f64() * 1000.0);
        if answer.rows.len() != k {
            return Err(format!(
                "the direct query answered {} rows, not {k}",
                answer.rows.len()
            ));
        }
    }
    let planned_median = middle(&mut planned);
    let direct_median = middle(&mut direct);
    println!();
    println!(
        "  {:<34} {:>9} {:>9} {:>9}",
        "arm", "p50 ms", "p90 ms", "p99 ms"
    );
    println!(
        "  {:<34} {:>9.3} {:>9.3} {:>9.3}",
        "the store's own query",
        direct_median,
        at(&direct, 0.90),
        at(&direct, 0.99)
    );
    println!(
        "  {:<34} {:>9.3} {:>9.3} {:>9.3}",
        "ORDER BY vector_distance_cos",
        planned_median,
        at(&planned, 0.90),
        at(&planned, 0.99)
    );
    println!(
        "  the SQL path adds {:.3} ms at p50, which is {:.2}x",
        planned_median - direct_median,
        if direct_median > 0.0 {
            planned_median / direct_median
        } else {
            0.0
        }
    );
    println!("  the difference is the planner match, k rowid descents, and the exact rescore");
    let wanted = probes.saturating_mul(k).max(1);
    println!(
        "  recall of the planned query against an exhaustive cosine: {:.3} ({found} of {wanted})",
        found as f64 / wanted as f64
    );
    Ok(())
}

/// Returns the ids of the `k` rows nearest a probe, by a cosine computed here.
///
/// Regenerates the corpus from its seeds rather than reading it back, so the
/// answer does not depend on anything the engine stored.
///
/// @param seed - the probe vector's seed
/// @param rows - how many vectors the corpus holds
/// @param dims - how wide each one is
/// @param k - how many neighbours
fn nearest(seed: u64, rows: usize, dims: usize, k: usize) -> Vec<i64> {
    let probe = vector_of(seed, dims);
    let mut ranked: Vec<(f64, usize)> = (0..rows)
        .map(|index| {
            (
                cosine_distance(&vector_of(index as u64, dims), &probe),
                index,
            )
        })
        .collect();
    ranked.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.1.cmp(&right.1))
    });
    ranked
        .iter()
        .take(k)
        .map(|(_, index)| *index as i64)
        .collect()
}

/// Returns one minus the cosine similarity of two vectors.
///
/// @param left - one vector
/// @param right - the other, of the same width
fn cosine_distance(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (a, b) in left.iter().zip(right) {
        dot += f64::from(*a) * f64::from(*b);
        left_norm += f64::from(*a) * f64::from(*a);
        right_norm += f64::from(*b) * f64::from(*b);
    }
    let scale = (left_norm * right_norm).sqrt();
    if scale == 0.0 {
        return 1.0;
    }
    1.0 - dot / scale
}

/// Counts how many of a query's rows are among the true nearest.
///
/// @param rows - the planned query's rows, whose first column is the id
/// @param wanted - the ids an exhaustive cosine picks
fn shared_neighbours(rows: &[Vec<inillucent_tree::datum::OwnedDatum>], wanted: &[i64]) -> usize {
    rows.iter()
        .filter(|row| match row.first() {
            Some(inillucent_tree::datum::OwnedDatum::Int(id)) => wanted.contains(id),
            _ => false,
        })
        .count()
}

/// Returns a flag's value, when it was given.
///
/// @param arguments - the command line
/// @param name - the flag, with its dashes
fn flag(arguments: &[String], name: &str) -> Option<usize> {
    let at = arguments.iter().position(|value| value == name)?;
    arguments.get(at.saturating_add(1))?.parse().ok()
}
