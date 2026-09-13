//! Measures what a commit pays to keep a `inillucent_search` index current, as a
//! function of corpus size - the number `docs/roadmap.md` item 10 and this
//! ticket (task-1911) are both about.
//!
//! Invariant: **the arms differ in the engine and in nothing else.** Both run
//! the same documents through the same module entry point in the same order,
//! so a difference between two runs of this harness is the change under test
//! or it is the machine, and the three corpus sizes are what tell those apart:
//! a cost that is proportional to the corpus shows as a slope, and the box's
//! own noise does not.
//!
//! Invariant this harness exists to check: **before task-1911, publishing a
//! generation read and rewrote the whole thing, so the worst commit's cost
//! rose with the corpus. After it, a commit that flushes builds and writes
//! only its own batch, so the worst commit's cost should no longer rise the
//! same way.** Comparing this harness's own output before and after the
//! change - by reverting `crates/inillucent-search` to `HEAD`, rebuilding
//! this binary, and running it again - is how `task-1911`'s report states
//! the claim as a table rather than an assertion.
//!
//! **Why this is a bin and not a `#[test]`.** It drives the module directly
//! through `Module`/`VirtualTable`, the same entry point a SQL engine uses,
//! over an in-memory `ShadowStore` - so it depends on nothing outside this
//! crate plus `inillucent-ext`/`inillucent-base`/`inillucent-value`, and
//! never touches `inillucent-compat`, which is mid a large, unrelated
//! rewrite at the time this was written. It holds no `#[test]`, so it needs
//! no row in `tests/selection.toml` - the same reason
//! `inillucent-compat/src/bin/foldgate.rs` has none: `selection.rs` only
//! requires a row for a target that carries a `#[test]`.
//!
//! Usage: `cargo run --release -p inillucent-search --bin write_latency`
//! (`--documents N` for one size, otherwise the three sizes the report uses).

// This is a measurement harness, not product code: it panics on a setup
// failure it cannot recover from, the same allowance
// `inillucent-compat/src/differential.rs` gives itself and for the same
// reason - a harness that could not open its own store is a broken
// environment, not a result to report.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_ext::vtab::{
    Change, Context, Host, Module, ModuleArguments, ShadowRoot, ShadowStore,
};
use inillucent_search::module::SearchModule;
use inillucent_search::store::{state, Store, SUFFIXES};
use inillucent_value::Value;

/// One shadow table's rows, kept both ways `ShadowStore` can be asked for
/// them - by rowid, for `%_content`/`%_delta`/`%_gen`, and by a leading key,
/// for `%_config`/`%_state`. A real backing store picks the shape its schema
/// needs; this one just keeps both, because the module - not this harness -
/// decides which table is which.
#[derive(Default)]
struct RootData {
    rows: BTreeMap<i64, Vec<Value<'static>>>,
    keyed: HashMap<Vec<u8>, Vec<Value<'static>>>,
}

/// An in-memory `ShadowStore`, so this harness measures the module's own
/// cost - graph work and byte serialisation - without a real pager, WAL or
/// disk underneath it adding noise neither arm of a before/after comparison
/// would want counted.
#[derive(Default)]
struct MemStore {
    roots: HashMap<u32, RootData>,
}

impl MemStore {
    fn root(&mut self, root: u32) -> &mut RootData {
        self.roots.entry(root).or_default()
    }
}

/// Encodes the leading key columns of a keyed row into bytes a `HashMap` can
/// hold, tagging each value's kind so two different types never collide.
/// @param values - the row, or just the key
/// @param key_columns - how many leading values form the key
fn encode_key(values: &[Value<'static>], key_columns: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values.iter().take(key_columns) {
        match value {
            Value::Text(text) => {
                out.push(1u8);
                out.extend_from_slice(text.raw());
            }
            Value::Integer(number) => {
                out.push(2u8);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Blob(blob) => {
                out.push(3u8);
                out.extend_from_slice(blob.raw());
            }
            Value::Real(number) => {
                out.push(4u8);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Null => out.push(0u8),
        }
    }
    out
}

impl ShadowStore for MemStore {
    fn read_row(&mut self, root: u32, rowid: i64) -> DbResult<Option<Vec<Value<'static>>>> {
        Ok(self.root(root).rows.get(&rowid).cloned())
    }

    fn write_row(&mut self, root: u32, rowid: i64, values: &[Value<'static>]) -> DbResult<()> {
        self.root(root).rows.insert(rowid, values.to_vec());
        Ok(())
    }

    fn delete_row(&mut self, root: u32, rowid: i64) -> DbResult<()> {
        self.root(root).rows.remove(&rowid);
        Ok(())
    }

    fn max_rowid(&mut self, root: u32) -> DbResult<i64> {
        Ok(self
            .root(root)
            .rows
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0))
    }

    fn scan(
        &mut self,
        root: u32,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let rows: Vec<(i64, Vec<Value<'static>>)> = self
            .root(root)
            .rows
            .iter()
            .map(|(rowid, values)| (*rowid, values.clone()))
            .collect();
        for (rowid, values) in rows {
            if !body(rowid, &values)? {
                break;
            }
        }
        Ok(())
    }

    fn scan_from(
        &mut self,
        root: u32,
        from: i64,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let rows: Vec<(i64, Vec<Value<'static>>)> = self
            .root(root)
            .rows
            .range(from..)
            .map(|(rowid, values)| (*rowid, values.clone()))
            .collect();
        for (rowid, values) in rows {
            if !body(rowid, &values)? {
                break;
            }
        }
        Ok(())
    }

    fn read_keyed(
        &mut self,
        root: u32,
        key: &[Value<'static>],
        _columns: usize,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let key = encode_key(key, key.len());
        Ok(self.root(root).keyed.get(&key).cloned())
    }

    fn write_keyed(
        &mut self,
        root: u32,
        key_columns: usize,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let key = encode_key(values, key_columns);
        self.root(root).keyed.insert(key, values.to_vec());
        Ok(())
    }

    fn delete_keyed(&mut self, root: u32, key: &[Value<'static>]) -> DbResult<()> {
        let key = encode_key(key, key.len());
        self.root(root).keyed.remove(&key);
        Ok(())
    }

    fn scan_keyed(
        &mut self,
        root: u32,
        _key_columns: usize,
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let rows: Vec<Vec<Value<'static>>> = self.root(root).keyed.values().cloned().collect();
        for values in rows {
            if !body(&values)? {
                break;
            }
        }
        Ok(())
    }
}

/// Hands the module the in-memory store above.
struct MemHost {
    store: MemStore,
}

impl Host for MemHost {
    fn shadow_store(&mut self) -> Option<&mut dyn ShadowStore> {
        Some(&mut self.store)
    }
}

/// Returns the module arguments one benchmark table connects with.
///
/// `dims = 64` and `mode = 'approximate'` match
/// `inillucent-compat/src/bin/foldgate.rs`'s own declaration, so a number
/// measured here sits on the same footing as the ones `docs/roadmap.md`
/// item 10 already published. No `compact` or `segment_merge` override - the
/// point is the *default* rule, `max(1024, rows / 8)`, which is what an
/// application that declares nothing gets.
fn arguments() -> ModuleArguments {
    ModuleArguments {
        database: 0,
        schema: b"main".to_vec(),
        table: b"docs".to_vec(),
        module: b"inillucent_search".to_vec(),
        arguments: vec![
            b"title".to_vec(),
            b"body".to_vec(),
            b"dims = 64".to_vec(),
            b"mode = 'approximate'".to_vec(),
        ],
        shadows: SUFFIXES
            .iter()
            .enumerate()
            .map(|(index, suffix)| ShadowRoot {
                suffix: suffix.to_vec(),
                root: index as u32 + 1,
            })
            .collect(),
    }
}

/// Returns a deterministic vector for one row - the same generator
/// `foldgate.rs` uses, so the two harnesses index the same shape of corpus.
/// @param id - the rowid
/// @param dims - how wide the vector column is
fn vector_for(id: i64, dims: usize) -> Vec<f32> {
    let mut seed = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1);
    let mut out = Vec::with_capacity(dims);
    for _ in 0..dims {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push(((seed >> 40) as f32 / 16_777_216.0) - 0.5);
    }
    out
}

/// Encodes a vector as the little-endian blob `store::encode_vector` would
/// have produced from SQL, without going through SQL to get one.
/// @param vector - the vector
fn vector_blob(vector: &[f32]) -> Value<'static> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Value::owned_blob(&bytes).expect("a vector blob encodes")
}

/// Builds one row's declared-column values: title, body, then the hidden
/// query/limit/vector/recall/rank columns in the order `module.rs` declares
/// them, vector aside every other hidden column NULL because an insert never
/// reads them.
/// @param id - the rowid
/// @param dims - how wide the vector column is
fn row_values(id: i64, dims: usize) -> Vec<Value<'static>> {
    vec![
        Value::owned_text(format!("title {id}").as_bytes()).expect("text encodes"),
        Value::owned_text(format!("body of document number {id}").as_bytes())
            .expect("text encodes"),
        Value::Null,
        Value::Null,
        vector_blob(&vector_for(id, dims)),
        Value::Null,
        Value::Null,
    ]
}

/// One corpus size's measurement.
struct Measurement {
    documents: usize,
    commits_ms: Vec<f64>,
    rows: i64,
    chunks: i64,
    folds: i64,
}

/// Writes `documents` rows, one per commit, timing every one.
/// @param documents - how many rows to write
fn run(documents: usize) -> Measurement {
    let mut host = MemHost {
        store: MemStore::default(),
    };
    let limits = Limits::default();
    let module_arguments = arguments();
    let mut table = SearchModule
        .connect(&module_arguments, true)
        .expect("the table connects");

    let mut commits_ms = Vec::with_capacity(documents);
    for id in 1..=documents as i64 {
        let mut context = Context {
            host: &mut host,
            database: 0,
            limits: &limits,
            catalog: None,
        };
        table.begin(&mut context).expect("the transaction begins");
        let started = Instant::now();
        table
            .update(
                &mut context,
                &Change::Insert {
                    rowid: Value::Integer(id),
                    values: row_values(id, 64),
                },
            )
            .expect("the insert is applied");
        table.sync(&mut context).expect("the transaction syncs");
        table.commit(&mut context).expect("the transaction commits");
        commits_ms.push(started.elapsed().as_secs_f64() * 1e3);
    }

    // Read the counters back the same way an application would, through a
    // fresh `Store` over the same backing rows - the table above owns no
    // state a second handle cannot also see, because all of it lives in
    // `%_state`.
    let store = Store::of(&module_arguments, 2).expect("the store reopens");
    let mut context = Context {
        host: &mut host,
        database: 0,
        limits: &limits,
        catalog: None,
    };
    let rows = store.state(&mut context, state::ROWS).unwrap_or(-1);
    let chunks = store.state(&mut context, state::CHUNKS).unwrap_or(-1);
    let folds = store.state(&mut context, state::FOLDS).unwrap_or(-1);

    Measurement {
        documents,
        commits_ms,
        rows,
        chunks,
        folds,
    }
}

/// Returns a percentile of a set of readings.
/// @param readings - the readings, in any order
/// @param share - the percentile, from 0 to 1
fn percentile(readings: &[f64], share: f64) -> f64 {
    if readings.is_empty() {
        return 0.0;
    }
    let mut sorted = readings.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let last = sorted.len().saturating_sub(1);
    let position = ((last as f64) * share).round() as usize;
    sorted.get(position.min(last)).copied().unwrap_or(0.0)
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let sizes: Vec<usize> = match flag(&arguments, "--documents") {
        Some(n) => vec![n],
        None => vec![1_000, 10_000, 100_000],
    };

    println!("documents,commit_p50_ms,commit_p99_ms,commit_worst_ms,rows,chunks,folds");
    for documents in sizes {
        let measurement = run(documents);
        println!(
            "{},{:.4},{:.4},{:.4},{},{},{}",
            measurement.documents,
            percentile(&measurement.commits_ms, 0.50),
            percentile(&measurement.commits_ms, 0.99),
            percentile(&measurement.commits_ms, 1.00),
            measurement.rows,
            measurement.chunks,
            measurement.folds,
        );
    }
}

/// Returns the value of a `--name value` flag as a number.
/// @param arguments - the command line
/// @param name - the flag
fn flag(arguments: &[String], name: &str) -> Option<usize> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1))?.parse().ok()
}
