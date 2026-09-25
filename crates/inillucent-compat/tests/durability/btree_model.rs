//! The B-tree property program: generated operation sequences, compared with
//! an independent model after every single step.
//!
//! Invariant: the comparison happens after *every* operation, not at the end.
//! A B-tree that loses a row on the fourth split and gains one back on the
//! ninth passes an end-to-end check and is still broken, and the only way to
//! know which operation did it is to have looked after the one before.
//!
//! Three things are checked, and they are independent of each other on purpose:
//!
//! 1. the logical contents against [`ModelBTree`], which is a sorted map and
//!    knows nothing about pages;
//! 2. the structure against the raw integrity checker, which walks the file
//!    without using a cursor, so a bug in cursor logic cannot validate itself;
//! 3. page ownership - every page in the file is in exactly one tree, chain,
//!    freelist or map - which is what the integrity check's accounting pass
//!    does, and is the property a leaked or doubly-owned page violates.
//!
//! A failing sequence is shrunk by removing operations while the failure
//! survives, and the minimal sequence is written to `compat/corpus/btree/` and
//! replayed by [`every_retained_sequence_still_passes`] on every later run. A
//! failure that has been seen once is a failure that is checked for ever.

use std::path::{Path, PathBuf};

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::page::PageSize;
use inillucent_compat::model::{generate, ModelBTree, ModelKey, ModelKind, Op, Shape};
use inillucent_compat::workspace_root;
use inillucent_storage::check::{self, CheckOptions};
use inillucent_storage::cursor::{BTreeCursor, SeekBias};
use inillucent_storage::header::VacuumMode;
use inillucent_storage::mutate;
use inillucent_storage::pager::{NewDatabase, Pager, PagerOptions};
use inillucent_storage::vacuum;
use inillucent_value::record::{encode_record, KeyInfo, RecordRef};
use inillucent_value::{BlobValue, TextEncoding, Value};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::DbPath;

/// Runs one sequence and returns what went wrong, if anything.
struct Harness<'a> {
    vfs: &'a MemoryVfs,
    path: DbPath,
    pager: Pager,
    root: PageId,
    model: ModelBTree,
    shape: Shape,
    savepoints: usize,
    step: usize,
}

impl<'a> Harness<'a> {
    /// Creates the database and the empty tree the sequence runs against.
    fn start(vfs: &'a MemoryVfs, shape: Shape) -> Result<Harness<'a>, String> {
        let path = DbPath::new("model.db");
        let mut pager = Pager::create(
            vfs,
            &path,
            PagerOptions::default(),
            NewDatabase {
                page_size: PageSize::new(shape.page_size).map_err(|error| error.to_string())?,
                reserved_bytes: 0,
                text_encoding: TextEncoding::Utf8,
                vacuum_mode: if shape.vacuum {
                    VacuumMode::Incremental
                } else {
                    VacuumMode::None
                },
            },
        )
        .map_err(|error| error.to_string())?;
        pager.begin_write().map_err(|error| error.to_string())?;
        let root = match shape.kind {
            ModelKind::Table => mutate::create_table(&mut pager),
            ModelKind::Index => mutate::create_index(&mut pager),
        }
        .map_err(|error| error.to_string())?;
        pager.commit().map_err(|error| error.to_string())?;
        pager.begin_write().map_err(|error| error.to_string())?;
        Ok(Harness {
            vfs,
            path,
            pager,
            root,
            model: ModelBTree::new(shape.kind),
            shape,
            savepoints: 0,
            step: 0,
        })
    }

    /// Returns the model key an operation's key names.
    fn key_of(&self, key: i64) -> ModelKey {
        match self.shape.kind {
            ModelKind::Table => ModelKey::Rowid(key),
            ModelKind::Index => ModelKey::Bytes(key.to_be_bytes().to_vec()),
        }
    }

    /// Builds the payload an insert stores.
    ///
    /// The bytes are derived from the key so that a row read back can be told
    /// apart from a different row of the same length - a check that compares
    /// only lengths passes when two rows have been swapped.
    fn payload_of(&self, key: i64, len: usize) -> Result<Vec<u8>, String> {
        let filler: Vec<u8> = (0..len)
            .map(|index| (key as u8).wrapping_add(index as u8))
            .collect();
        let key_bytes = key.to_be_bytes();
        let values = match self.shape.kind {
            ModelKind::Table => vec![Value::Blob(BlobValue::borrowed(&filler))],
            ModelKind::Index => vec![
                Value::Blob(BlobValue::borrowed(&key_bytes)),
                Value::Blob(BlobValue::borrowed(&filler)),
            ],
        };
        encode_record(&values, TextEncoding::Utf8, 4).map_err(|error| error.to_string())
    }

    /// Reads the whole tree in cursor order.
    fn scan(&mut self) -> Result<Vec<(ModelKey, Vec<u8>)>, String> {
        let limits = Limits::default();
        let mut out = Vec::new();
        match self.shape.kind {
            ModelKind::Table => {
                let mut cursor = BTreeCursor::table(self.root);
                let mut more = cursor.first(&mut self.pager).map_err(text)?;
                while more {
                    let rowid = cursor.rowid().map_err(text)?;
                    let payload = cursor.payload(&mut self.pager, &limits).map_err(text)?;
                    out.push((ModelKey::Rowid(rowid), payload));
                    more = cursor.next(&mut self.pager).map_err(text)?;
                }
            }
            ModelKind::Index => {
                let mut cursor = BTreeCursor::index(self.root, KeyInfo::binary(2));
                let mut more = cursor.first(&mut self.pager).map_err(text)?;
                while more {
                    let payload = cursor.payload(&mut self.pager, &limits).map_err(text)?;
                    out.push((index_key(&payload)?, payload));
                    more = cursor.next(&mut self.pager).map_err(text)?;
                }
            }
        }
        Ok(out)
    }

    /// Compares the tree with the model, naming the first difference.
    fn compare(&mut self) -> Result<(), String> {
        let found = self.scan()?;
        let wanted = self.model.entries();
        if found.len() != wanted.len() {
            return Err(format!(
                "the tree holds {} entries and the model holds {}",
                found.len(),
                wanted.len()
            ));
        }
        for (index, (found_entry, wanted_entry)) in found.iter().zip(wanted.iter()).enumerate() {
            if found_entry.0 != wanted_entry.0 {
                return Err(format!(
                    "entry {index} has key {:?} in the tree and {:?} in the model",
                    found_entry.0, wanted_entry.0
                ));
            }
            if found_entry.1 != wanted_entry.1 {
                return Err(format!(
                    "entry {index} with key {:?} has a payload of {} bytes in the tree and {} in the model",
                    found_entry.0,
                    found_entry.1.len(),
                    wanted_entry.1.len()
                ));
            }
        }
        Ok(())
    }

    /// Runs the raw integrity check over the tree and the whole file.
    fn check(&mut self) -> Result<(), String> {
        let mut options = CheckOptions::roots(vec![self.root]);
        if self.shape.kind == ModelKind::Index {
            options = options.with_key(self.root, KeyInfo::binary(2));
        }
        let report = check::check_database_with_options(&mut self.pager, &options).map_err(text)?;
        if report.is_ok() {
            return Ok(());
        }
        Err(format!("integrity check: {:?}", report.as_pragma_output()))
    }

    /// Applies one operation to both the tree and the model.
    fn apply(&mut self, op: &Op) -> Result<(), String> {
        match op {
            Op::Insert { key, payload_len } => {
                let model_key = self.key_of(*key);
                let payload = self.payload_of(*key, *payload_len)?;
                match self.shape.kind {
                    ModelKind::Table => {
                        mutate::insert_row(&mut self.pager, self.root, *key, &payload)
                            .map_err(text)?;
                    }
                    ModelKind::Index => {
                        // An index entry is identified by its whole record, so
                        // replacing one means removing the record that is there
                        // and inserting the new one - which is what an index
                        // maintainer above would do too.
                        if let Some((_, existing)) = self
                            .model
                            .entries()
                            .into_iter()
                            .find(|(existing, _)| *existing == model_key)
                        {
                            mutate::delete_entry(
                                &mut self.pager,
                                self.root,
                                &KeyInfo::binary(2),
                                &existing,
                            )
                            .map_err(text)?;
                        }
                        mutate::insert_entry(
                            &mut self.pager,
                            self.root,
                            &KeyInfo::binary(2),
                            &payload,
                        )
                        .map_err(text)?;
                    }
                }
                self.model.insert(model_key, payload);
            }
            Op::Delete { key } => {
                let model_key = self.key_of(*key);
                let expected = self.model.contains(&model_key);
                let removed = match self.shape.kind {
                    ModelKind::Table => {
                        mutate::delete_row(&mut self.pager, self.root, *key).map_err(text)?
                    }
                    ModelKind::Index => {
                        let record = match self
                            .model
                            .entries()
                            .into_iter()
                            .find(|(existing, _)| *existing == model_key)
                        {
                            Some((_, record)) => record,
                            None => self.payload_of(*key, 8)?,
                        };
                        mutate::delete_entry(
                            &mut self.pager,
                            self.root,
                            &KeyInfo::binary(2),
                            &record,
                        )
                        .map_err(text)?
                    }
                };
                if removed != expected {
                    return Err(format!(
                        "deleting {model_key:?} reported {removed} and the model said {expected}"
                    ));
                }
                self.model.delete(&model_key);
            }
            Op::Scan => {
                self.compare()?;
            }
            Op::Range { low, high } => {
                if self.shape.kind == ModelKind::Table {
                    let found = self.rowid_range(*low, *high)?;
                    let wanted = self.model.rowid_range(*low, *high);
                    if found != wanted {
                        return Err(format!(
                            "the range {low}..={high} holds {} entries in the tree and {} in the model",
                            found.len(),
                            wanted.len()
                        ));
                    }
                }
            }
            Op::CursorRestore { key } => self.cursor_restore(*key)?,
            Op::Savepoint => {
                let name = format!("s{}", self.savepoints);
                self.pager.begin_savepoint(&name).map_err(text)?;
                self.model.begin_savepoint();
                self.savepoints = self.savepoints.saturating_add(1);
            }
            Op::RollbackSavepoint => {
                if self.savepoints > 0 {
                    self.savepoints = self.savepoints.saturating_sub(1);
                    let name = format!("s{}", self.savepoints);
                    self.pager.rollback_to_savepoint(&name).map_err(text)?;
                    self.pager.release_savepoint(&name).map_err(text)?;
                    self.model.rollback_savepoint();
                }
            }
            Op::ReleaseSavepoint => {
                if self.savepoints > 0 {
                    self.savepoints = self.savepoints.saturating_sub(1);
                    let name = format!("s{}", self.savepoints);
                    self.pager.release_savepoint(&name).map_err(text)?;
                    self.model.release_savepoint();
                }
            }
            Op::Commit => {
                self.pager.commit().map_err(text)?;
                self.model.commit();
                self.savepoints = 0;
                self.check()?;
                self.pager.begin_write().map_err(text)?;
            }
            Op::Rollback => {
                self.pager.rollback().map_err(text)?;
                self.model.rollback();
                self.savepoints = 0;
                self.check()?;
                self.pager.begin_write().map_err(text)?;
            }
            Op::Reopen => {
                // Closing rolls back, which is what a process that stops
                // without committing leaves behind.
                self.pager.rollback().map_err(text)?;
                self.pager.close().map_err(text)?;
                self.model.rollback();
                self.savepoints = 0;
                self.pager = Pager::open_read_write(self.vfs, &self.path, PagerOptions::default())
                    .map_err(text)?;
                self.pager.begin_read().map_err(text)?;
                self.check()?;
                self.pager.begin_write().map_err(text)?;
            }
            Op::Vacuum { steps } => {
                if self.shape.vacuum {
                    vacuum::incremental_vacuum(&mut self.pager, *steps).map_err(text)?;
                }
            }
        }
        Ok(())
    }

    /// Reads a range of rowids through a seek and a scan.
    fn rowid_range(&mut self, low: i64, high: i64) -> Result<Vec<(ModelKey, Vec<u8>)>, String> {
        let limits = Limits::default();
        let mut cursor = BTreeCursor::table(self.root);
        let mut out = Vec::new();
        let mut more = cursor
            .seek_rowid(&mut self.pager, low, SeekBias::AtOrAfter)
            .map_err(text)?
            || cursor.is_positioned();
        while more {
            let rowid = cursor.rowid().map_err(text)?;
            if rowid > high {
                break;
            }
            if rowid >= low {
                let payload = cursor.payload(&mut self.pager, &limits).map_err(text)?;
                out.push((ModelKey::Rowid(rowid), payload));
            }
            more = cursor.next(&mut self.pager).map_err(text)?;
        }
        Ok(out)
    }

    /// Positions a cursor, changes the tree under it, and puts it back.
    fn cursor_restore(&mut self, key: i64) -> Result<(), String> {
        if self.shape.kind != ModelKind::Table {
            return Ok(());
        }
        let limits = Limits::default();
        let mut cursor = BTreeCursor::table(self.root);
        let found = cursor
            .seek_rowid(&mut self.pager, key, SeekBias::AtOrAfter)
            .map_err(text)?;
        if !cursor.is_positioned() {
            return Ok(());
        }
        let saved = cursor
            .save_position(&mut self.pager, &limits)
            .map_err(text)?;
        let landed = cursor.rowid().map_err(text)?;

        // Something that reshapes the tree underneath the cursor.
        let filler = self.payload_of(key, self.shape.page_size as usize)?;
        let scratch = key.saturating_add(1_000_000);
        mutate::insert_row(&mut self.pager, self.root, scratch, &filler).map_err(text)?;
        self.model.insert(self.key_of(scratch), filler);

        let back = cursor
            .restore(&mut self.pager, &saved, &limits)
            .map_err(text)?;
        if back != found && found {
            return Err(format!(
                "a cursor on rowid {landed} could not be restored to it"
            ));
        }
        if cursor.is_positioned() {
            let now = cursor.rowid().map_err(text)?;
            if found && now != landed {
                return Err(format!(
                    "a restored cursor landed on rowid {now} instead of {landed}"
                ));
            }
        }
        Ok(())
    }
}

/// Returns the eight-byte key an index record's first field holds.
fn index_key(payload: &[u8]) -> Result<ModelKey, String> {
    let limits = Limits::default();
    let record =
        RecordRef::parse_with_limits(payload, TextEncoding::Utf8, &limits).map_err(text)?;
    let value = record.value(0).map_err(text)?;
    match value {
        Value::Blob(blob) => Ok(ModelKey::Bytes(blob.raw().to_vec())),
        other => Err(format!("an index key that is not a blob: {other:?}")),
    }
}

/// Renders an error as a string.
///
/// A `DbError`'s `Display` deliberately omits its internal detail so that
/// logging one cannot leak a path or a bound value. A test failure needs the
/// detail - "a cell claiming 39 local bytes past the page" is the whole
/// diagnosis - so the harness formats it with `Debug` instead.
fn text(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

/// Runs a whole sequence, returning the step and the reason it failed.
fn run(shape: Shape, ops: &[Op]) -> Result<(), String> {
    let vfs = MemoryVfs::new();
    let mut harness = Harness::start(&vfs, shape)?;
    for (index, op) in ops.iter().enumerate() {
        harness.step = index;
        harness
            .apply(op)
            .map_err(|reason| format!("step {index} ({}): {reason}", op.name()))?;
        harness
            .compare()
            .map_err(|reason| format!("after step {index} ({}): {reason}", op.name()))?;
    }
    harness.pager.commit().map_err(text)?;
    harness.model.commit();
    harness
        .check()
        .map_err(|reason| format!("after the final commit: {reason}"))?;
    harness
        .compare()
        .map_err(|reason| format!("after the final commit: {reason}"))
}

/// Removes operations while the failure survives, returning the shortest
/// sequence that still fails.
///
/// The page size and the order of what is left are never changed: a shrinker
/// that reordered operations would produce a sequence that fails for a
/// different reason than the one it was given.
fn shrink(shape: Shape, ops: &[Op]) -> Vec<Op> {
    let mut best = ops.to_vec();
    let mut changed = true;
    let mut rounds = 0;
    while changed && rounds < 6 {
        rounds += 1;
        changed = false;
        let mut index = 0;
        while index < best.len() {
            let mut candidate = best.clone();
            candidate.remove(index);
            if run(shape, &candidate).is_err() {
                best = candidate;
                changed = true;
            } else {
                index += 1;
            }
        }
    }
    best
}

/// Returns the directory retained failing sequences live in.
fn corpus_directory() -> PathBuf {
    workspace_root().join("compat/corpus/btree")
}

/// Writes a failing sequence where a later run will replay it.
fn retain(name: &str, shape: Shape, ops: &[Op]) -> PathBuf {
    let directory = corpus_directory();
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}.jsonl"));
    let mut text = format!(
        "{{\"page_size\":{},\"kind\":\"{}\",\"vacuum\":{}}}\n",
        shape.page_size,
        match shape.kind {
            ModelKind::Table => "table",
            ModelKind::Index => "index",
        },
        shape.vacuum
    );
    for op in ops {
        text.push_str(&op.to_json());
        text.push('\n');
    }
    let _ = std::fs::write(&path, text);
    path
}

/// Runs one shape at one seed, shrinking and retaining anything that fails.
fn exercise(seed: u64, shape: Shape) {
    let ops = generate(seed, shape);
    let Err(reason) = run(shape, &ops) else {
        return;
    };
    let minimal = shrink(shape, &ops);
    let name = format!(
        "p{}-{}-seed{seed}",
        shape.page_size,
        match shape.kind {
            ModelKind::Table => "table",
            ModelKind::Index => "index",
        }
    );
    let path = retain(&name, shape, &minimal);
    panic!(
        "seed {seed} at page size {} failed: {reason}\n\
         the shrunk sequence is {} operations and was written to {}",
        shape.page_size,
        minimal.len(),
        path.display()
    );
}

/// Generated table sequences match the model at every step, at every page size.
#[test]
fn generated_table_sequences_match_the_model() {
    for page_size in [512u32, 1024, 4096, 65_536] {
        let seeds: &[u64] = if page_size == 65_536 {
            &[1, 2]
        } else {
            &[1, 2, 3, 4, 5, 6, 7, 8]
        };
        let length = if page_size == 65_536 { 120 } else { 300 };
        for seed in seeds.iter().copied() {
            exercise(
                seed,
                Shape {
                    page_size,
                    length,
                    key_range: 400,
                    kind: ModelKind::Table,
                    vacuum: false,
                },
            );
        }
    }
}

/// Generated index sequences match the model at every step, at every page size.
#[test]
fn generated_index_sequences_match_the_model() {
    for page_size in [512u32, 1024, 4096, 65_536] {
        let seeds: &[u64] = if page_size == 65_536 {
            &[11]
        } else {
            &[11, 12, 13, 14, 15, 16]
        };
        let length = if page_size == 65_536 { 100 } else { 250 };
        for seed in seeds.iter().copied() {
            exercise(
                seed,
                Shape {
                    page_size,
                    length,
                    key_range: 300,
                    kind: ModelKind::Index,
                    vacuum: false,
                },
            );
        }
    }
}

/// The same, on an auto-vacuum database, where pages move under the tree.
#[test]
fn generated_sequences_match_the_model_under_a_vacuum() {
    for page_size in [512u32, 1024, 4096] {
        for seed in [21u64, 22, 23, 24] {
            exercise(
                seed,
                Shape {
                    page_size,
                    length: 250,
                    key_range: 250,
                    kind: ModelKind::Table,
                    vacuum: true,
                },
            );
        }
    }
}

/// A sequence made only of the operations that stress the local-payload
/// boundary, at every page size, so the threshold is exercised deliberately
/// rather than by chance.
#[test]
fn payloads_at_the_local_boundary_match_the_model() {
    for page_size in [512u32, 1024, 4096, 65_536] {
        let usable = page_size as usize;
        let max_local = usable.saturating_sub(35);
        let mut ops = Vec::new();
        for key in 0..40i64 {
            for delta in [-1isize, 0, 1] {
                let payload_len = max_local.saturating_add_signed(delta);
                ops.push(Op::Insert {
                    key: key.saturating_mul(3).saturating_add(delta as i64 + 1),
                    payload_len,
                });
            }
        }
        ops.push(Op::Commit);
        for key in (0..120i64).step_by(2) {
            ops.push(Op::Delete { key });
        }
        ops.push(Op::Commit);
        let shape = Shape {
            page_size,
            length: ops.len(),
            key_range: 200,
            kind: ModelKind::Table,
            vacuum: false,
        };
        if let Err(reason) = run(shape, &ops) {
            panic!("page size {page_size}: {reason}");
        }
    }
}

/// Every sequence that has ever failed is replayed on every run.
#[test]
fn every_retained_sequence_still_passes() {
    let directory = corpus_directory();
    if !directory.is_dir() {
        inillucent_compat::differential::skipping(&format!(
            "no retained sequences at {}",
            directory.display()
        ));
        return;
    }
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => panic!("cannot read {}: {error}", directory.display()),
    };
    let mut replayed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let (shape, ops) = load(&path);
        if let Err(reason) = run(shape, &ops) {
            panic!("{} still fails: {reason}", path.display());
        }
        replayed += 1;
    }
    // **A loop over a directory that asserts per item and never asserts the
    // count (task-1969, 4.8).** Renaming the two files under `compat/corpus/btree/`
    // left the directory present and empty, so the skip above did not fire, the
    // loop replayed nothing, and the case printed `replayed 0` and passed. Rule
    // 1.2 of the testing standard names this shape; `policy.rs`'s
    // `assert!(checked >= 40, ...)` and `syntax.rs`'s `assert!(compared > 200, ...)`
    // are the pattern.
    assert!(
        replayed > 0,
        "the retained corpus at {} holds no .jsonl sequence, so this replayed nothing",
        directory.display()
    );
    println!("replayed {replayed} retained sequences");
}

/// Reads a retained sequence back.
fn load(path: &Path) -> (Shape, Vec<Op>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => panic!("cannot read {}: {error}", path.display()),
    };
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    let page_size = header
        .split("\"page_size\":")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse::<u32>().ok())
        .unwrap_or(4096);
    let kind = if header.contains("\"kind\":\"index\"") {
        ModelKind::Index
    } else {
        ModelKind::Table
    };
    let vacuum = header.contains("\"vacuum\":true");
    let ops: Vec<Op> = lines.filter_map(Op::parse).collect();
    (
        Shape {
            page_size,
            length: ops.len(),
            key_range: 400,
            kind,
            vacuum,
        },
        ops,
    )
}
