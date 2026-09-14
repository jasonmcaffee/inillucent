//! The five shadow tables a search index lives in, and nothing else.
//!
//! Invariant: **every durable byte of a search index is an ordinary row in an
//! ordinary b-tree inside the database it belongs to.** There is no side file,
//! no directory, no daemon and no second write path. That single decision is
//! what makes the acceptance criterion of this phase true by construction
//! rather than by argument: relational rows and search entries commit together
//! because they are committed by the same pager transaction, roll back together
//! because they are undone by the same journal, and survive a crash together
//! because they are recovered by the same recovery.
//!
//! The tables are:
//!
//! - `%_config(k, v)` - the definition. Written once when the table is created
//!   and read every time it is connected: columns, vector width, distance,
//!   tokenizer identity, exact-or-approximate, format version.
//! - `%_content(id, c0..cN, v)` - the indexed row versions. This is the
//!   authoritative copy of every row: the base generation and the delta log are
//!   both derivable from it, which is what makes `rebuild` a real operation and
//!   not a hope.
//! - `%_delta(seq, commit_seq, id, op, digest)` - the durable delta log. One row
//!   per change since the base generation's covered sequence, in commit order,
//!   each carrying the one commit sequence its transaction published.
//! - `%_gen(id, generation, ordinal, bytes)` - the immutable base generations,
//!   each a `inillucent_core::persist` byte stream cut into rows. A generation is
//!   never overwritten and never automatically removed.
//! - `%_state(k, v)` - which generation is current, how far it covers, the next
//!   commit sequence, the live row count, and the build progress marker.
//!
//! What a reader has to merge is therefore small and bounded: one immutable
//! generation plus at most `compact` delta rows.

use inillucent_base::DbResult;
use inillucent_ext::shadow::ShadowTables;
use inillucent_ext::vtab::{failure, Context, ModuleArguments, ShadowTable};
use inillucent_value::Value;

/// The suffixes of the tables a search index owns, in the order they are made.
pub const SUFFIXES: [&[u8]; 5] = [b"config", b"content", b"delta", b"gen", b"state"];

/// How many bytes of a generation stream go into one `%_gen` row.
///
/// Large enough that a big index is a few hundred rows rather than a hundred
/// thousand, small enough that one row is not an overflow chain the length of
/// the file. The number is not load bearing; the format records nothing about
/// it, and a generation written with a different chunk size reads back the
/// same.
pub const CHUNK: usize = 512 * 1024;

/// A key in `%_state`.
pub mod state {
    /// The last commit sequence published, shared by every change one
    /// transaction made.
    pub const SEQUENCE: &str = "sequence";
    /// The last row ordinal handed out in the delta log.
    ///
    /// Separate from the commit sequence because the log's primary key has to
    /// be unique per row while the commit sequence is deliberately shared: ten
    /// rows written by one transaction are ten log rows carrying one sequence.
    pub const ORDINAL: &str = "ordinal";
    /// The generation the base index was published under.
    pub const GENERATION: &str = "generation";
    /// The highest delta sequence the base generation already contains.
    pub const COVERED: &str = "covered";
    /// How many live rows `%_content` holds.
    pub const ROWS: &str = "rows";
    /// How far a rebuild got, so a resumed one knows where it was.
    pub const BUILD: &str = "build";
    /// How many chunks the last generation build inserted into the graph.
    ///
    /// **This is the number the bound is stated in.** A
    /// generation folded at commit inserts the rows that commit wrote; a
    /// generation built by the `compact` or `rebuild` command inserts the whole
    /// corpus. Reading it back is how an application, and the cost guard in
    /// `crates/inillucent/tests/budget.rs`, tells the two apart without a
    /// stopwatch.
    pub const INSERTED: &str = "inserted";
    /// How many folds the current generation lineage has taken.
    ///
    /// Reset to zero by `compact` and by `rebuild`, because both build the
    /// graph in one pass from the rows. It rises by one per folded commit, and
    /// a large number next to a large `chunks` minus `rows` is what says the
    /// graph has accumulated enough tombstoned chunks to be worth rebuilding.
    pub const FOLDS: &str = "folds";
    /// How many chunks the current generation holds, live and tombstoned.
    ///
    /// `chunks` minus `rows` is the dead weight an incremental update leaves
    /// behind: an update tombstones the old chunk and appends a new one, and
    /// only a single-pass build removes the old one. Under segmented
    /// generations this is the sum of every live segment's own chunk count -
    /// see [`crate::store::SegmentMeta::chunks`].
    pub const CHUNKS: &str = "chunks";
    /// The segment manifest: which immutable segments are live, in the order
    /// a query has to fold them in.
    ///
    /// Encoded by `encode_segments` and stored as a blob rather than an
    /// integer, which is why it lives beside the other counters instead of
    /// inside them - `%_state` is an ordinary key/value table and a blob is
    /// just another value. Its **absence** is meaningful: a table written
    /// before task-1911 has no row under this key at all, and
    /// `crate::module::SearchTable::live_segments` reads that as "one
    /// segment, the one `GENERATION` and `COVERED` already name" rather than
    /// as zero segments - see the module for why that distinction is the one
    /// that must never read as "no rows".
    pub const SEGMENTS: &str = "segments";
    /// The next fresh identifier for a `%_gen` row group.
    ///
    /// Separate from `GENERATION`, which counts *build events* - flushes,
    /// compactions, rebuilds - because a segment merge is deliberately
    /// invisible to that counter (see the module's `merge_cascade`) and yet
    /// still needs a storage identifier of its own that cannot collide with
    /// one still referenced by the manifest. Left at zero until the first
    /// segment this build ever allocates one for; a table migrating up from a
    /// single generation seeds it from the highest number already in
    /// `%_gen` or already named by `GENERATION`, whichever is larger, so the
    /// first new segment a migrated table writes can never land on a rowid an
    /// old, undropped generation is still using.
    pub const SEGMENT_ID: &str = "segment_id";
    /// Every in-flight segment merge that ran out of its per commit budget
    /// before it finished, encoded by `encode_merge_states`.
    ///
    /// **A list, because more than one level can be merging at once** - a
    /// table under enough write pressure to have two levels both over
    /// `Options::segment_fanin` at the same time makes progress on both
    /// rather than starving one while the other resumes. **Absence means
    /// nothing is waiting to be resumed** - a table that has never started a
    /// merge, or whose last commit finished every one it was running, has no
    /// row here at all. `crate::module::SearchTable::merge_cascade` is the
    /// only reader and writer of this row that matters: it resumes whatever
    /// is here before it starts anything new, and removes an entry the
    /// moment that merge finishes. `compact` and `rebuild` also clear the
    /// whole row, because both replace the whole manifest a merge in
    /// progress was only ever collapsing part of.
    pub const MERGE: &str = "merge";
    /// How many chunks the most recent commit's own call to
    /// `crate::module::SearchTable::merge_cascade` folded while merging
    /// segments.
    ///
    /// **This is the number the per commit bound is stated in** - the same
    /// role [`INSERTED`] plays for a flush or a full build. It is overwritten
    /// on every commit, including one that touched no merge at all (reading
    /// `0` then), so it always reports the most recent commit's own share
    /// rather than a running total, and a test can read it back to check the
    /// bound was respected without a clock: it should never exceed
    /// `Options::merge_budget_chunks` by more than one segment's worth, the
    /// "at least one folds" progress guarantee `merge_cascade`'s own doc
    /// comment describes - except during a crisis merge, which is allowed to
    /// spend past it on purpose.
    pub const MERGE_WORK: &str = "merge_work";
}

/// One immutable segment of a search index's built structure.
///
/// A segment is a `inillucent_core::index::Index`, serialised exactly as the
/// single generation this format replaces always was - `%_gen` does not know
/// or care whether the bytes under one id are the whole corpus or one
/// commit's batch. What is new is that several of these can be live at once,
/// each covering a disjoint, contiguous run of the delta log, and a query
/// folds them in order rather than reading one.
///
/// `covers_from` and `covers_to` are what make the fold order the same thing
/// as recency: the runs never overlap and never leave a gap, so sorting
/// segments by `covers_from` is sorting them from oldest to newest, and an id
/// touched by more than one segment is decided by whichever one's range is
/// furthest to the right - not by which one happens to have the largest
/// stored id, which a merge event deliberately reuses out of order (see
/// `module::SearchTable::merge_cascade`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentMeta {
    /// The key into `%_gen` this segment's bytes are stored under.
    pub id: i64,
    /// How many times this segment has already been through a merge.
    ///
    /// Purely a bucket for the merge policy - `crate::options::Options`'s
    /// `segment_merge` - to decide when a level is full. It plays no part in
    /// deciding recency; `covers_from`/`covers_to` already do that, and doing
    /// it twice in two different fields would let them disagree.
    pub level: i32,
    /// One below the lowest delta sequence this segment's content reflects.
    pub covers_from: i64,
    /// The highest delta sequence this segment's content reflects.
    pub covers_to: i64,
    /// How many chunks this segment's own index holds, live and tombstoned.
    ///
    /// Cached here rather than read back from the segment's bytes so that
    /// `state::CHUNKS` can be kept current by summing this field over the
    /// live manifest, without deserialising a segment nobody asked to search.
    pub chunks: i64,
    /// Rows this segment's range deleted, that never had a live chunk of
    /// their own *within this segment* to carry the fact.
    ///
    /// A row that was written and then deleted inside the same flushed batch
    /// never becomes a chunk at all - the batch is collapsed to its last
    /// operation per id before a segment is built from it - so there is
    /// nothing in the segment's own index for a fold to find and skip. This
    /// list is the only durable record that the id is dead as of this
    /// segment, and a fold that skipped reading it would let an older
    /// segment's stale row answer again. Ascending, so a search over it does
    /// not have to be a scan.
    pub tombstoned: Vec<i64>,
}

/// A segment merge whose per commit budget ran out before it finished, and
/// which the next commit resumes rather than restarting.
///
/// Every field is fixed at the moment the merge began except `folded` and
/// `accumulator`, which move forward one commit at a time -
/// `crate::module::SearchTable::continue_merge` is the only writer. Persisted
/// under [`state::MERGE`] rather than folded into the segment manifest,
/// because a half finished merge must never be read as though it were live:
/// [`crate::merge::live_segments`] answers from the manifest alone, and this
/// is invisible to it until `crate::module::SearchTable::finish_merge` swaps
/// the finished segment in and removes this row - so a query, and a crash,
/// only ever see every original input still live or the one segment that
/// replaced them, never a mixture of the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeState {
    /// The level being collapsed.
    pub source_level: i32,
    /// The level the merged output lands at.
    pub target_level: i32,
    /// Every segment this merge started with, oldest first, fixed for the
    /// merge's whole life - a segment written to `source_level` after this
    /// merge began plays no part in it and is picked up by a later one.
    pub inputs: Vec<SegmentMeta>,
    /// How many of `inputs`, from the front, are already folded into
    /// `accumulator`.
    pub folded: usize,
    /// The `%_gen` id currently holding the accumulator's bytes.
    ///
    /// A real, durable segment, but not named by the live manifest. Costs
    /// nothing to reach the first time: the oldest input becomes the
    /// accumulator directly, so `folded == 1` and `accumulator` naming that
    /// same input's own existing id is the merge's starting state, before a
    /// single byte has been written on its account.
    pub accumulator: i64,
}

/// What one delta row records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// The row named by `id` was written, and `%_content` holds it.
    Put,
    /// The row named by `id` was removed, and `%_content` does not hold it.
    Delete,
}

impl Op {
    /// Returns the integer the row stores.
    pub fn code(self) -> i64 {
        match self {
            Op::Put => 1,
            Op::Delete => 2,
        }
    }

    /// Reads an op back, treating anything unrecognised as a delete.
    ///
    /// A delete is the safe reading of a corrupt op code: it removes a row from
    /// the answers rather than inventing one, and the row itself is still in
    /// `%_content` for `integrity-check` to find.
    pub fn from_code(code: i64) -> Op {
        if code == 1 {
            Op::Put
        } else {
            Op::Delete
        }
    }
}

/// One entry of the delta log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delta {
    /// The log row's own ordinal, which is its primary key.
    pub sequence: i64,
    /// The commit sequence the change was published under.
    ///
    /// Every change one transaction made carries the same number, and the
    /// relational rows the same transaction wrote became visible at the same
    /// instant. That is what "one commit sequence shared by table rows and
    /// search delta metadata" means here, and it is checkable: a test can read
    /// the log and see one number per transaction.
    pub commit: i64,
    /// The row the change is about.
    pub id: i64,
    /// What happened to it.
    pub op: Op,
    /// A digest of the row as it was written, or zero for a delete.
    ///
    /// This is what makes a cached merge safe to extend rather than rebuild.
    /// Sequence numbers come out of a shadow row, so a rolled-back transaction
    /// gives its numbers back and a later one can reuse them for different
    /// content; comparing sequences alone would then accept a cached index
    /// built from rows that no longer exist. Comparing the digest as well makes
    /// the check content-addressed.
    pub digest: i64,
}

/// One row of a search table, as `%_content` holds it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// The text columns, in declaration order.
    pub columns: Vec<String>,
    /// The row's vector, empty when the table has no vector branch.
    pub vector: Vec<f32>,
}

impl Row {
    /// Returns a digest of the row's content.
    ///
    /// FNV-1a over the encoded columns and vector. It identifies a row version,
    /// which is all it is asked to do; it is not a security property and
    /// nothing depends on it being hard to collide deliberately, because
    /// anything that could choose a colliding row could equally well write the
    /// row it wanted directly.
    pub fn digest(&self) -> i64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for column in &self.columns {
            eat(&(column.len() as u64).to_le_bytes());
            eat(column.as_bytes());
        }
        eat(&(self.vector.len() as u64).to_le_bytes());
        for value in &self.vector {
            eat(&value.to_le_bytes());
        }
        // Cast rather than truncate: the whole 64 bits travel, and an i64 is
        // what a database row can hold.
        hash as i64
    }
}

/// Returns the `CREATE TABLE` statements a search table's shadow tables need.
///
/// `%_content` has one column per declared column, so the statement is built
/// rather than constant. Everything else is fixed.
/// @param columns - how many text columns the table declares
pub fn shadow_tables(columns: usize) -> Vec<ShadowTable> {
    let mut content = String::from("CREATE TABLE \"%_content\"(id INTEGER PRIMARY KEY");
    for index in 0..columns {
        content.push_str(&format!(", c{index}"));
    }
    content.push_str(", v)");
    vec![
        ShadowTable {
            suffix: b"config".to_vec(),
            create_sql: "CREATE TABLE \"%_config\"(k PRIMARY KEY, v) WITHOUT ROWID".to_string(),
            owner: None,
        },
        ShadowTable {
            suffix: b"content".to_vec(),
            create_sql: content,
            owner: None,
        },
        ShadowTable {
            suffix: b"delta".to_vec(),
            create_sql:
                "CREATE TABLE \"%_delta\"(seq INTEGER PRIMARY KEY, commit_seq, id, op, digest)"
                    .to_string(),
            owner: None,
        },
        ShadowTable {
            suffix: b"gen".to_vec(),
            create_sql:
                "CREATE TABLE \"%_gen\"(id INTEGER PRIMARY KEY, generation, ordinal, bytes)"
                    .to_string(),
            owner: None,
        },
        ShadowTable {
            suffix: b"state".to_vec(),
            create_sql: "CREATE TABLE \"%_state\"(k PRIMARY KEY, v) WITHOUT ROWID".to_string(),
            owner: None,
        },
    ]
}

/// One search table's shadow storage, and the only way this crate reaches it.
#[derive(Clone, Debug)]
pub struct Store {
    tables: ShadowTables,
    columns: usize,
}

impl Store {
    /// Returns the store over the roots the host handed the module.
    pub fn of(arguments: &ModuleArguments, columns: usize) -> DbResult<Store> {
        Ok(Store {
            tables: ShadowTables::of(arguments, &SUFFIXES)?,
            columns,
        })
    }

    /// Returns how many text columns a stored row has.
    pub fn columns(&self) -> usize {
        self.columns
    }

    // -- the definition ----------------------------------------------------

    /// Writes the whole definition, which happens once.
    pub fn write_config(
        &self,
        context: &mut Context<'_>,
        rows: &[(String, String)],
    ) -> DbResult<()> {
        for (key, value) in rows {
            self.tables.write_keyed(
                context,
                b"config",
                1,
                &[
                    Value::owned_text(key.as_bytes())?,
                    Value::owned_text(value.as_bytes())?,
                ],
            )?;
        }
        Ok(())
    }

    /// Reads the definition back.
    pub fn read_config(&self, context: &mut Context<'_>) -> DbResult<Vec<(String, String)>> {
        let mut rows = Vec::new();
        self.tables.scan_keyed(context, b"config", 1, |values| {
            let key = text_of(values.first());
            let value = text_of(values.get(1));
            if let Some(key) = key {
                rows.push((key, value.unwrap_or_default()));
            }
            Ok(true)
        })?;
        Ok(rows)
    }

    // -- the state ---------------------------------------------------------

    /// Reads one state counter, or zero when it has never been written.
    pub fn state(&self, context: &mut Context<'_>, key: &str) -> DbResult<i64> {
        let found =
            self.tables
                .read_keyed(context, b"state", &[Value::owned_text(key.as_bytes())?], 2)?;
        Ok(found
            .and_then(|values| values.get(1).and_then(Value::as_integer))
            .unwrap_or(0))
    }

    /// Writes one state counter.
    pub fn set_state(&self, context: &mut Context<'_>, key: &str, value: i64) -> DbResult<()> {
        self.tables.write_keyed(
            context,
            b"state",
            1,
            &[Value::owned_text(key.as_bytes())?, Value::Integer(value)],
        )
    }

    // -- the rows ----------------------------------------------------------

    /// Reads one row, or nothing when there is not one.
    pub fn read_row(&self, context: &mut Context<'_>, id: i64) -> DbResult<Option<Row>> {
        let Some(values) = self.tables.read_row(context, b"content", id)? else {
            return Ok(None);
        };
        Ok(Some(decode_row(&values, self.columns)))
    }

    /// Writes one row, replacing whatever was there.
    pub fn write_row(&self, context: &mut Context<'_>, id: i64, row: &Row) -> DbResult<()> {
        let mut values: Vec<Value<'static>> = Vec::with_capacity(self.columns + 2);
        values.push(Value::Integer(id));
        for index in 0..self.columns {
            match row.columns.get(index) {
                Some(text) => values.push(Value::owned_text(text.as_bytes())?),
                None => values.push(Value::Null),
            }
        }
        values.push(encode_vector(&row.vector)?);
        self.tables.write_row(context, b"content", id, &values)
    }

    /// Removes one row.
    pub fn delete_row(&self, context: &mut Context<'_>, id: i64) -> DbResult<()> {
        self.tables.delete_row(context, b"content", id)
    }

    /// Runs a body over every row, in rowid order, stopping when it says so.
    pub fn scan_rows(
        &self,
        context: &mut Context<'_>,
        mut body: impl FnMut(i64, Row) -> DbResult<bool>,
    ) -> DbResult<()> {
        let columns = self.columns;
        self.tables.scan(context, b"content", |id, values| {
            body(id, decode_row(values, columns))
        })
    }

    /// Returns the rowids the table holds, ascending.
    pub fn row_ids(&self, context: &mut Context<'_>) -> DbResult<Vec<i64>> {
        let mut ids = Vec::new();
        self.tables.scan(context, b"content", |id, _| {
            ids.push(id);
            Ok(true)
        })?;
        Ok(ids)
    }

    /// Returns the largest rowid `%_content` holds.
    pub fn max_row_id(&self, context: &mut Context<'_>) -> DbResult<i64> {
        self.tables.max_rowid(context, b"content")
    }

    // -- the delta log -----------------------------------------------------

    /// Appends one entry to the delta log.
    pub fn append_delta(&self, context: &mut Context<'_>, entry: Delta) -> DbResult<()> {
        self.tables.write_row(
            context,
            b"delta",
            entry.sequence,
            &[
                Value::Integer(entry.sequence),
                Value::Integer(entry.commit),
                Value::Integer(entry.id),
                Value::Integer(entry.op.code()),
                Value::Integer(entry.digest),
            ],
        )
    }

    /// Returns every delta entry above a sequence, in order.
    ///
    /// **A seek to `covered + 1`, not a scan of the whole log filtered by
    /// it.** `%_delta` is keyed by `seq`, which is handed out in ascending
    /// order, so the log is already in the order this wants and every entry
    /// at or below `covered` is one a base generation has already folded in -
    /// dead weight this used to read and decode on every call regardless.
    /// `fold` and `compact` reach this once per commit, so a full-table scan
    /// here read and threw away the whole delta log's history on every
    /// commit a table made, however small the pending batch actually was.
    pub fn deltas_above(&self, context: &mut Context<'_>, covered: i64) -> DbResult<Vec<Delta>> {
        let mut found = Vec::new();
        self.tables.scan_from(
            context,
            b"delta",
            covered.saturating_add(1),
            |sequence, values| {
                found.push(Delta {
                    sequence,
                    commit: values.get(1).and_then(Value::as_integer).unwrap_or(0),
                    id: values.get(2).and_then(Value::as_integer).unwrap_or(0),
                    op: Op::from_code(values.get(3).and_then(Value::as_integer).unwrap_or(2)),
                    digest: values.get(4).and_then(Value::as_integer).unwrap_or(0),
                });
                Ok(true)
            },
        )?;
        Ok(found)
    }

    /// Removes every delta entry at or below a sequence.
    pub fn forget_deltas(&self, context: &mut Context<'_>, covered: i64) -> DbResult<()> {
        let mut doomed = Vec::new();
        self.tables.scan(context, b"delta", |sequence, _| {
            if sequence > covered {
                return Ok(false);
            }
            doomed.push(sequence);
            Ok(true)
        })?;
        for sequence in doomed {
            self.tables.delete_row(context, b"delta", sequence)?;
        }
        Ok(())
    }

    // -- the generations ---------------------------------------------------

    /// Writes one generation's bytes as a run of rows.
    ///
    /// The rows are appended above whatever is already there, so publishing a
    /// new generation never touches the rows of the old one. That is what lets
    /// the old generation stay readable by a snapshot that is still using it,
    /// and it is why removing one is a separate, explicit act.
    pub fn write_generation(
        &self,
        context: &mut Context<'_>,
        generation: i64,
        bytes: &[u8],
    ) -> DbResult<()> {
        let mut next = self.tables.max_rowid(context, b"gen")?.saturating_add(1);
        for (ordinal, chunk) in bytes.chunks(CHUNK).enumerate() {
            self.tables.write_row(
                context,
                b"gen",
                next,
                &[
                    Value::Integer(next),
                    Value::Integer(generation),
                    Value::Integer(ordinal as i64),
                    Value::owned_blob(chunk)?,
                ],
            )?;
            next = next.saturating_add(1);
        }
        Ok(())
    }

    /// Reads one generation's bytes back, or nothing when it is not there.
    pub fn read_generation(
        &self,
        context: &mut Context<'_>,
        generation: i64,
    ) -> DbResult<Option<Vec<u8>>> {
        let mut parts: Vec<(i64, Vec<u8>)> = Vec::new();
        self.tables.scan(context, b"gen", |_, values| {
            if values.get(1).and_then(Value::as_integer) != Some(generation) {
                return Ok(true);
            }
            let ordinal = values.get(2).and_then(Value::as_integer).unwrap_or(0);
            let bytes = values
                .get(3)
                .and_then(Value::as_blob)
                .map(|blob| blob.raw().to_vec())
                .unwrap_or_default();
            parts.push((ordinal, bytes));
            Ok(true)
        })?;
        if parts.is_empty() {
            return Ok(None);
        }
        parts.sort_by_key(|(ordinal, _)| *ordinal);
        let mut bytes = Vec::new();
        for (_, part) in parts {
            bytes.extend_from_slice(&part);
        }
        Ok(Some(bytes))
    }

    /// Returns every generation the table still holds, ascending.
    pub fn generations(&self, context: &mut Context<'_>) -> DbResult<Vec<i64>> {
        let mut found: Vec<i64> = Vec::new();
        self.tables.scan(context, b"gen", |_, values| {
            if let Some(generation) = values.get(1).and_then(Value::as_integer) {
                if !found.contains(&generation) {
                    found.push(generation);
                }
            }
            Ok(true)
        })?;
        found.sort_unstable();
        Ok(found)
    }

    /// Removes every generation not named by a set of ids to keep.
    ///
    /// Never called by a write, only by the explicit maintenance command. A
    /// segment is immutable and a reader may still be inside one, so
    /// reclaiming the space is a decision an application makes rather than a
    /// side effect of an insert - a merge that has just folded three segments
    /// into one leaves the three old ones in the file for exactly this
    /// reason. `keep` names every id the current manifest still refers to,
    /// which is a set rather than a threshold because a merge's own id is not
    /// ordered against the ids of segments that outlived it: a segment merged
    /// early can end up with a smaller id than one still holding recent,
    /// unmerged rows.
    /// @param keep - every segment id the live manifest still refers to
    pub fn drop_generations_except(
        &self,
        context: &mut Context<'_>,
        keep: &[i64],
    ) -> DbResult<usize> {
        let mut doomed = Vec::new();
        self.tables.scan(context, b"gen", |rowid, values| {
            if values
                .get(1)
                .and_then(Value::as_integer)
                .is_some_and(|generation| !keep.contains(&generation))
            {
                doomed.push(rowid);
            }
            Ok(true)
        })?;
        let removed = doomed.len();
        for rowid in doomed {
            self.tables.delete_row(context, b"gen", rowid)?;
        }
        Ok(removed)
    }

    // -- the segment manifest ------------------------------------------------

    /// Reads one state row as a blob, or nothing when it has never been
    /// written.
    /// @param key - the state key
    fn state_blob(&self, context: &mut Context<'_>, key: &str) -> DbResult<Option<Vec<u8>>> {
        let found =
            self.tables
                .read_keyed(context, b"state", &[Value::owned_text(key.as_bytes())?], 2)?;
        Ok(found.and_then(|values| {
            values
                .get(1)
                .and_then(Value::as_blob)
                .map(|blob| blob.raw().to_vec())
        }))
    }

    /// Writes one state row as a blob.
    /// @param key - the state key
    /// @param bytes - the value
    fn set_state_blob(&self, context: &mut Context<'_>, key: &str, bytes: &[u8]) -> DbResult<()> {
        self.tables.write_keyed(
            context,
            b"state",
            1,
            &[
                Value::owned_text(key.as_bytes())?,
                Value::owned_blob(bytes)?,
            ],
        )
    }

    /// Reads the segment manifest, or `None` when the table has never written
    /// one.
    ///
    /// **`None` is not "zero segments".** A table written before segmented
    /// generations existed has no [`state::SEGMENTS`] row at all, and its one
    /// generation - named by [`state::GENERATION`] and covering up to
    /// [`state::COVERED`] - is still exactly one live segment; synthesising
    /// that here would put the same "is this table's history there or not"
    /// question in two places, so the caller does it once, in
    /// `module::SearchTable::live_segments`.
    pub fn read_segments(&self, context: &mut Context<'_>) -> DbResult<Option<Vec<SegmentMeta>>> {
        match self.state_blob(context, state::SEGMENTS)? {
            Some(bytes) => Ok(Some(decode_segments(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Writes the segment manifest back.
    /// @param segments - every live segment, in any order
    pub fn write_segments(
        &self,
        context: &mut Context<'_>,
        segments: &[SegmentMeta],
    ) -> DbResult<()> {
        self.set_state_blob(context, state::SEGMENTS, &encode_segments(segments))
    }

    // -- the in-flight merges ------------------------------------------------

    /// Reads every in-flight merge, or an empty list when none is waiting to
    /// be resumed.
    ///
    /// **A list, not one merge**, because a level's own merge and a level
    /// above or below it merging at the same time are independent: a table
    /// under enough write pressure to have two levels both over
    /// `Options::segment_fanin` at once must make progress on both rather
    /// than starve one while the other resumes, which is what stalled the
    /// bounded merge's own tail before this - see
    /// `crate::module::SearchTable::merge_cascade`'s own doc comment.
    pub fn read_merge_states(&self, context: &mut Context<'_>) -> DbResult<Vec<MergeState>> {
        match self.state_blob(context, state::MERGE)? {
            Some(bytes) => decode_merge_states(&bytes),
            None => Ok(Vec::new()),
        }
    }

    /// Writes every in-flight merge back, so a later commit can resume each
    /// of them rather than starting over. An empty list clears the row
    /// entirely rather than storing an empty count, so a table with nothing
    /// in flight goes back to having no [`state::MERGE`] row at all.
    /// @param merge_states - every merge still in progress, in any order
    pub fn write_merge_states(
        &self,
        context: &mut Context<'_>,
        merge_states: &[MergeState],
    ) -> DbResult<()> {
        if merge_states.is_empty() {
            return self.clear_merge_states(context);
        }
        self.set_state_blob(context, state::MERGE, &encode_merge_states(merge_states))
    }

    /// Removes every in-flight merge - called once the last one finishes,
    /// and by `compact`/`rebuild`, which replace the whole manifest a merge
    /// in progress was only ever collapsing part of.
    pub fn clear_merge_states(&self, context: &mut Context<'_>) -> DbResult<()> {
        self.tables.delete_keyed(
            context,
            b"state",
            &[Value::owned_text(state::MERGE.as_bytes())?],
        )
    }
}

/// Encodes the segment manifest as a fixed-width blob.
///
/// Hand rolled rather than borrowing a serialisation crate, the same choice
/// `encode_vector` already made for the same reason: this is a handful of
/// integers and a per-segment tombstone list, and a general purpose format
/// would cost a dependency to save writing four `to_le_bytes` calls.
/// @param segments - every live segment
pub fn encode_segments(segments: &[SegmentMeta]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + segments.len() * 40);
    bytes.extend_from_slice(&(segments.len() as u64).to_le_bytes());
    for segment in segments {
        bytes.extend_from_slice(&segment.id.to_le_bytes());
        bytes.extend_from_slice(&i64::from(segment.level).to_le_bytes());
        bytes.extend_from_slice(&segment.covers_from.to_le_bytes());
        bytes.extend_from_slice(&segment.covers_to.to_le_bytes());
        bytes.extend_from_slice(&segment.chunks.to_le_bytes());
        bytes.extend_from_slice(&(segment.tombstoned.len() as u64).to_le_bytes());
        for id in &segment.tombstoned {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
    }
    bytes
}

/// Reads one little-endian `i64` from a byte cursor, refusing a truncated
/// buffer rather than panicking or indexing past the end.
///
/// Shared by every decoder that reads a `%_state` blob - `decode_segments`
/// and `decode_merge_state` both read a database page's bytes back, so both
/// follow the "no unwrap, no indexing" rule the same way. Each names its own
/// failure message, so a corrupt manifest and a corrupt merge state are still
/// told apart at the point they are reported rather than both reading as one
/// generic complaint.
/// @param bytes - the buffer being read
/// @param at - the cursor, advanced past what was read
/// @param message - what to report if the buffer runs out here
fn take_i64(bytes: &[u8], at: &mut usize, message: &'static str) -> DbResult<i64> {
    let corrupt = || failure(message);
    let word = bytes.get(*at..at.saturating_add(8)).ok_or_else(corrupt)?;
    let array: [u8; 8] = word.try_into().map_err(|_| corrupt())?;
    *at = at.saturating_add(8);
    Ok(i64::from_le_bytes(array))
}

/// Reads the segment manifest starting at a cursor, advancing it past what
/// was read.
///
/// The shared implementation behind [`decode_segments`] (which starts at
/// zero and discards the final position) and [`decode_merge_states`] (which
/// reads several of these back to back, each needing to know exactly where
/// the next one starts).
/// @param bytes - the buffer being read
/// @param at - the cursor, advanced past what was read
fn decode_segments_at(bytes: &[u8], at: &mut usize) -> DbResult<Vec<SegmentMeta>> {
    const MESSAGE: &str = "inillucent_search: a corrupt segment manifest";
    let count = take_i64(bytes, at, MESSAGE)? as u64;
    let mut segments = Vec::new();
    for _ in 0..count {
        let id = take_i64(bytes, at, MESSAGE)?;
        let level = take_i64(bytes, at, MESSAGE)? as i32;
        let covers_from = take_i64(bytes, at, MESSAGE)?;
        let covers_to = take_i64(bytes, at, MESSAGE)?;
        let chunks = take_i64(bytes, at, MESSAGE)?;
        let tombstoned_count = take_i64(bytes, at, MESSAGE)? as u64;
        let mut tombstoned = Vec::new();
        for _ in 0..tombstoned_count {
            tombstoned.push(take_i64(bytes, at, MESSAGE)?);
        }
        segments.push(SegmentMeta {
            id,
            level,
            covers_from,
            covers_to,
            chunks,
            tombstoned,
        });
    }
    Ok(segments)
}

/// Reads the segment manifest back, refusing truncated or malformed bytes
/// rather than panicking.
///
/// This reads a database page, so the "no unwrap, no indexing" rule applies:
/// a corrupt or foreshortened blob - truncated by a bug elsewhere, or by
/// somebody editing `%_state` by hand - is reported as
/// `inillucent_search: a corrupt segment manifest`, not a crash.
/// @param bytes - the stored blob
pub fn decode_segments(bytes: &[u8]) -> DbResult<Vec<SegmentMeta>> {
    let mut at = 0usize;
    decode_segments_at(bytes, &mut at)
}

/// Encodes an in-flight merge's state as a blob, reusing [`encode_segments`]
/// for the input list rather than inventing a second segment encoding.
/// @param merge_state - the merge to persist
pub fn encode_merge_state(merge_state: &MergeState) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(32 + merge_state.inputs.len() * 40);
    bytes.extend_from_slice(&i64::from(merge_state.source_level).to_le_bytes());
    bytes.extend_from_slice(&i64::from(merge_state.target_level).to_le_bytes());
    bytes.extend_from_slice(&(merge_state.folded as u64).to_le_bytes());
    bytes.extend_from_slice(&merge_state.accumulator.to_le_bytes());
    bytes.extend_from_slice(&encode_segments(&merge_state.inputs));
    bytes
}

/// Reads one in-flight merge's state starting at a cursor, advancing it past
/// what was read - the shared implementation behind [`decode_merge_state`]
/// and [`decode_merge_states`], for the same reason [`decode_segments_at`]
/// exists.
/// @param bytes - the buffer being read
/// @param at - the cursor, advanced past what was read
fn decode_merge_state_at(bytes: &[u8], at: &mut usize) -> DbResult<MergeState> {
    const MESSAGE: &str = "inillucent_search: a corrupt merge state";
    let source_level = take_i64(bytes, at, MESSAGE)? as i32;
    let target_level = take_i64(bytes, at, MESSAGE)? as i32;
    let folded = take_i64(bytes, at, MESSAGE)?.max(0) as usize;
    let accumulator = take_i64(bytes, at, MESSAGE)?;
    let inputs = decode_segments_at(bytes, at)?;
    Ok(MergeState {
        source_level,
        target_level,
        inputs,
        folded,
        accumulator,
    })
}

/// Reads an in-flight merge's state back, refusing truncated or malformed
/// bytes rather than panicking - the same rule [`decode_segments`] follows,
/// since this reads the same `%_state` blob storage.
/// @param bytes - the stored blob
pub fn decode_merge_state(bytes: &[u8]) -> DbResult<MergeState> {
    let mut at = 0usize;
    decode_merge_state_at(bytes, &mut at)
}

/// Encodes every in-flight merge as one blob: a count, then each one's own
/// [`encode_merge_state`] bytes back to back.
/// @param merge_states - every merge still in progress
pub fn encode_merge_states(merge_states: &[MergeState]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(merge_states.len() as u64).to_le_bytes());
    for merge_state in merge_states {
        bytes.extend_from_slice(&encode_merge_state(merge_state));
    }
    bytes
}

/// Reads every in-flight merge back, refusing truncated or malformed bytes
/// rather than panicking.
/// @param bytes - the stored blob
pub fn decode_merge_states(bytes: &[u8]) -> DbResult<Vec<MergeState>> {
    const MESSAGE: &str = "inillucent_search: a corrupt merge state list";
    let mut at = 0usize;
    let count = take_i64(bytes, &mut at, MESSAGE)? as u64;
    let mut merge_states = Vec::new();
    for _ in 0..count {
        merge_states.push(decode_merge_state_at(bytes, &mut at)?);
    }
    Ok(merge_states)
}

/// Returns the text of a value, or nothing when it is not text.
fn text_of(value: Option<&Value<'static>>) -> Option<String> {
    match value {
        Some(Value::Text(text)) => Some(String::from_utf8_lossy(text.raw()).into_owned()),
        Some(Value::Integer(number)) => Some(number.to_string()),
        Some(Value::Real(number)) => Some(number.to_string()),
        Some(Value::Blob(blob)) => Some(String::from_utf8_lossy(blob.raw()).into_owned()),
        _ => None,
    }
}

/// Turns a stored row back into columns and a vector.
fn decode_row(values: &[Value<'static>], columns: usize) -> Row {
    let mut text = Vec::with_capacity(columns);
    for index in 0..columns {
        text.push(text_of(values.get(index.saturating_add(1))).unwrap_or_default());
    }
    let vector = values
        .get(columns.saturating_add(1))
        .and_then(Value::as_blob)
        .map(|blob| decode_vector(blob.raw()))
        .unwrap_or_default();
    Row {
        columns: text,
        vector,
    }
}

/// Encodes a vector as the blob a row stores, or NULL when there is none.
///
/// Little-endian `f32`, which is the layout `inillucent_core` keeps vectors in and
/// the layout its saved files use, so a vector makes no round trip through a
/// different representation on its way in or out.
pub fn encode_vector(vector: &[f32]) -> DbResult<Value<'static>> {
    if vector.is_empty() {
        return Ok(Value::Null);
    }
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Value::owned_blob(&bytes)
}

/// Reads a vector back out of the blob a row stores.
pub fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    let mut vector = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let mut word = [0u8; 4];
        word.copy_from_slice(chunk);
        vector.push(f32::from_le_bytes(word));
    }
    vector
}

/// Reads a vector out of a bound value, refusing one of the wrong width.
///
/// A blob of the wrong length is a caller error rather than something to pad or
/// truncate: a query vector that is not the index's width cannot be compared
/// with anything in it, and quietly resizing it would return a ranking of
/// nonsense.
/// @param value - the bound value
/// @param dims - the width the table declared
pub fn vector_of(value: &Value<'static>, dims: usize) -> DbResult<Vec<f32>> {
    let vector = match value {
        Value::Null => return Ok(Vec::new()),
        Value::Blob(blob) => decode_vector(blob.raw()),
        Value::Text(text) => decode_vector(text.raw()),
        _ => {
            return Err(failure(
                "inillucent_search: a vector is a blob of little-endian 32-bit floats",
            ))
        }
    };
    if vector.len() != dims {
        return Err(failure(format!(
            "inillucent_search: this index has {dims} dimensions, and the vector has {}",
            vector.len()
        )));
    }
    // **A NaN is refused here, at the one place a vector enters the index
    // (task-1932, M4).** `hnsw.rs` orders candidates with
    // `partial_cmp(..).unwrap_or(Equal)`, which makes a NaN distance compare
    // equal to everything - and "equal to everything" is not a total order, so
    // the `BinaryHeap` the search walks is no longer a heap. What comes out is
    // not a wrong score but an arbitrary set of neighbours, and every later
    // query on the same graph is affected rather than the row that carried the
    // NaN. An infinity is the same argument one step less severe: it orders,
    // and it makes every distance through that node infinite.
    //
    // Refused with the same shape as the width check, because a caller that
    // handles one handles the other.
    if let Some(at) = vector.iter().position(|component| !component.is_finite()) {
        return Err(failure(format!(
            "inillucent_search: component {at} of this vector is {}, and a vector's \
             components have to be finite numbers",
            vector.get(at).copied().unwrap_or(f32::NAN)
        )));
    }
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vector survives the round trip through a row's blob unchanged.
    #[test]
    fn a_vector_round_trips_through_a_blob() {
        let vector = vec![0.5f32, -0.25, 1.0e-8, 12345.75];
        let encoded = encode_vector(&vector).expect("encoded");
        let blob = match &encoded {
            Value::Blob(blob) => blob.raw().to_vec(),
            _ => panic!("a vector encodes as a blob"),
        };
        assert_eq!(decode_vector(&blob), vector);
    }

    /// An empty vector is NULL rather than a zero-length blob, so a lexical
    /// table's rows do not each carry an empty allocation.
    #[test]
    fn no_vector_is_null() {
        assert!(matches!(encode_vector(&[]).expect("encoded"), Value::Null));
    }

    /// A query vector of the wrong width is refused rather than resized.
    #[test]
    fn a_vector_of_the_wrong_width_is_refused() {
        let value = encode_vector(&[1.0, 2.0]).expect("encoded");
        assert!(vector_of(&value, 2).is_ok());
        assert!(vector_of(&value, 3).is_err());
    }

    /// A vector holding a NaN or an infinity is refused.
    ///
    /// **The one that matters is the NaN (task-1932, M4).** `hnsw.rs` orders
    /// candidates with `partial_cmp(..).unwrap_or(Equal)`, so a NaN distance
    /// compares equal to everything - and "equal to everything" is not a total
    /// order, so the `BinaryHeap` the search walks stops being a heap. What
    /// comes out is not a wrong score for the row that carried the NaN; it is
    /// an arbitrary set of neighbours for every query after it.
    ///
    /// Both signs of infinity are refused for the smaller reason: they order,
    /// and they make every distance through that node infinite.
    #[test]
    fn a_vector_holding_a_non_finite_component_is_refused() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let value = encode_vector(&[1.0, bad, 3.0]).expect("encoded");
            let refused = vector_of(&value, 3)
                .err()
                .unwrap_or_else(|| panic!("{bad} was accepted as a vector component"));
            assert!(
                refused.message().contains("finite")
                    || refused.detail().is_some_and(|said| said.contains("finite")),
                "{bad} was refused for another reason: {}",
                refused.message()
            );
        }
        // And an ordinary vector of the same width still passes, so the check
        // is on the component rather than on the shape.
        let good = encode_vector(&[1.0, 2.0, 3.0]).expect("encoded");
        assert!(vector_of(&good, 3).is_ok());
    }

    /// Two different row versions get different digests.
    #[test]
    fn a_digest_identifies_a_row_version() {
        let first = Row {
            columns: vec!["hello".to_string()],
            vector: Vec::new(),
        };
        let second = Row {
            columns: vec!["hello there".to_string()],
            vector: Vec::new(),
        };
        assert_ne!(first.digest(), second.digest());
        assert_eq!(first.digest(), first.clone().digest());
    }

    /// The content table grows a column per declared column plus the vector.
    #[test]
    fn the_content_table_matches_the_declaration() {
        let tables = shadow_tables(2);
        let content = tables
            .iter()
            .find(|table| table.suffix == b"content")
            .expect("content");
        assert!(content.create_sql.contains("c0"));
        assert!(content.create_sql.contains("c1"));
        assert!(!content.create_sql.contains("c2"));
        assert_eq!(tables.len(), SUFFIXES.len());
    }

    /// A `ShadowStore` over one in-memory rowid table, for testing
    /// `deltas_above` without a real engine.
    ///
    /// **What it counts is the point.** Every row `scan` or `scan_from` hands
    /// to the caller's body increments `visited`, so the difference between a
    /// scan-then-filter and a seek shows up as a difference in this count
    /// rather than in a clock: `scan` always starts at the first row this
    /// store holds, and `scan_from` starts at `from` by using `BTreeMap`'s own
    /// range, the in-memory equivalent of the tree descent `visit_range`
    /// makes on a real store.
    #[derive(Default)]
    struct CountingStore {
        rows: std::collections::BTreeMap<i64, Vec<Value<'static>>>,
        /// How many rows a scan of either kind has handed to a body, across
        /// every call this store has answered.
        visited: usize,
    }

    impl inillucent_ext::vtab::ShadowStore for CountingStore {
        fn read_row(&mut self, _root: u32, rowid: i64) -> DbResult<Option<Vec<Value<'static>>>> {
            Ok(self.rows.get(&rowid).cloned())
        }

        fn write_row(&mut self, _root: u32, rowid: i64, values: &[Value<'static>]) -> DbResult<()> {
            self.rows.insert(rowid, values.to_vec());
            Ok(())
        }

        fn delete_row(&mut self, _root: u32, rowid: i64) -> DbResult<()> {
            self.rows.remove(&rowid);
            Ok(())
        }

        fn max_rowid(&mut self, _root: u32) -> DbResult<i64> {
            Ok(self.rows.keys().next_back().copied().unwrap_or(0))
        }

        fn scan(
            &mut self,
            _root: u32,
            body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
        ) -> DbResult<()> {
            let rows: Vec<(i64, Vec<Value<'static>>)> =
                self.rows.iter().map(|(k, v)| (*k, v.clone())).collect();
            for (rowid, values) in rows {
                self.visited = self.visited.saturating_add(1);
                if !body(rowid, &values)? {
                    break;
                }
            }
            Ok(())
        }

        fn scan_from(
            &mut self,
            _root: u32,
            from: i64,
            body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
        ) -> DbResult<()> {
            let rows: Vec<(i64, Vec<Value<'static>>)> = self
                .rows
                .range(from..)
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            for (rowid, values) in rows {
                self.visited = self.visited.saturating_add(1);
                if !body(rowid, &values)? {
                    break;
                }
            }
            Ok(())
        }

        fn read_keyed(
            &mut self,
            _root: u32,
            _key: &[Value<'static>],
            _columns: usize,
        ) -> DbResult<Option<Vec<Value<'static>>>> {
            Ok(None)
        }

        fn write_keyed(
            &mut self,
            _root: u32,
            _key_columns: usize,
            _values: &[Value<'static>],
        ) -> DbResult<()> {
            Ok(())
        }

        fn delete_keyed(&mut self, _root: u32, _key: &[Value<'static>]) -> DbResult<()> {
            Ok(())
        }

        fn scan_keyed(
            &mut self,
            _root: u32,
            _key_columns: usize,
            _body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
        ) -> DbResult<()> {
            Ok(())
        }
    }

    /// `deltas_above` seeks to `covered + 1` rather than reading and
    /// discarding everything at or below it.
    ///
    /// **A count, not a stopwatch** - the testing standard's rule 1.7. A
    /// scan-then-filter implementation visits every row in the delta log on
    /// every call, so a call with a low watermark and one with a high
    /// watermark over the *same* thousand-row log would visit the same
    /// thousand rows both times. A seek visits only what is above the
    /// watermark, so the second call here visits one row, not a thousand.
    /// Reverting `deltas_above` to `self.tables.scan(...)` filtered on
    /// `sequence > covered` makes this fail: it reports `2_000` visited in
    /// total where the seek reports `1_001`, and the message names the
    /// thousand it should not have read.
    #[test]
    fn deltas_above_seeks_past_the_watermark_instead_of_scanning_it() {
        let mut backing = CountingStore::default();
        for sequence in 1..=1_000i64 {
            backing.rows.insert(
                sequence,
                vec![
                    Value::Integer(sequence),
                    Value::Integer(sequence),
                    Value::Integer(1),
                    Value::Integer(Op::Put.code()),
                    Value::Integer(0),
                ],
            );
        }
        let arguments = ModuleArguments {
            database: 0,
            schema: b"main".to_vec(),
            table: b"t".to_vec(),
            module: b"inillucent_search".to_vec(),
            arguments: Vec::new(),
            shadows: SUFFIXES
                .iter()
                .enumerate()
                .map(|(index, suffix)| inillucent_ext::vtab::ShadowRoot {
                    suffix: suffix.to_vec(),
                    root: index as u32 + 1,
                })
                .collect(),
        };
        let table_store = Store::of(&arguments, 1).expect("store built");
        let mut host = inillucent_ext::vtab::WithStore { store: backing };
        let limits = inillucent_base::limits::Limits::default();

        let low = table_store
            .deltas_above(
                &mut Context {
                    host: &mut host,
                    database: 0,
                    limits: &limits,
                    catalog: None,
                },
                0,
            )
            .expect("scanned from the very start");
        assert_eq!(low.len(), 1_000, "everything above sequence 0 is every row");
        assert_eq!(
            host.store.visited, 1_000,
            "reading from the start visits every row once"
        );

        let high = table_store
            .deltas_above(
                &mut Context {
                    host: &mut host,
                    database: 0,
                    limits: &limits,
                    catalog: None,
                },
                999,
            )
            .expect("scanned from near the end");
        assert_eq!(high.len(), 1, "only the last row sorts above sequence 999");
        assert_eq!(
            host.store.visited, 1_001,
            "a seek to sequence 1000 visits the one row above it, not the \
             thousand a scan-then-filter would have read again to find it"
        );
    }

    /// A segment manifest round trips through its blob encoding exactly,
    /// tombstone lists included.
    ///
    /// **Fails without the change:** `SegmentMeta`, `encode_segments` and
    /// `decode_segments` do not exist before task-1911 - this test cannot
    /// compile against the code that came before it, which is the strongest
    /// version of "fails without the change" there is.
    #[test]
    fn a_segment_manifest_round_trips_through_its_blob() {
        let segments = vec![
            SegmentMeta {
                id: 1,
                level: 0,
                covers_from: 0,
                covers_to: 8,
                chunks: 8,
                tombstoned: Vec::new(),
            },
            SegmentMeta {
                id: 2,
                level: 0,
                covers_from: 8,
                covers_to: 20,
                chunks: 5,
                tombstoned: vec![3, 9, 17],
            },
        ];
        let bytes = encode_segments(&segments);
        let read_back = decode_segments(&bytes).expect("a well formed manifest decodes");
        assert_eq!(read_back, segments);
    }

    /// A truncated manifest blob is refused, not indexed into and not
    /// panicked on.
    ///
    /// This is the "no unwrap, no indexing, on a path that reads a page" rule
    /// applied to the one new binary format this ticket adds: a `%_state`
    /// row is exactly as untrusted as any other page, and a manifest cut
    /// short - by a bug, or by hand-editing the row - has to be reported as
    /// `inillucent_search: a corrupt segment manifest`, not crash the read.
    #[test]
    fn a_truncated_manifest_is_refused_not_panicked_on() {
        let segments = vec![SegmentMeta {
            id: 1,
            level: 0,
            covers_from: 0,
            covers_to: 8,
            chunks: 8,
            tombstoned: vec![2, 4],
        }];
        let mut bytes = encode_segments(&segments);
        bytes.truncate(bytes.len() - 3);
        let error = decode_segments(&bytes).expect_err("truncated bytes must not decode");
        assert!(
            format!("{error:?}").contains("corrupt segment manifest"),
            "{error:?}"
        );
    }

    /// An in-flight merge's state survives being written to a blob and read
    /// back, inputs and all.
    #[test]
    fn a_merge_state_round_trips_through_its_blob() {
        let state = MergeState {
            source_level: 0,
            target_level: 1,
            inputs: vec![
                SegmentMeta {
                    id: 5,
                    level: 0,
                    covers_from: 0,
                    covers_to: 1024,
                    chunks: 1024,
                    tombstoned: Vec::new(),
                },
                SegmentMeta {
                    id: 6,
                    level: 0,
                    covers_from: 1024,
                    covers_to: 2048,
                    chunks: 900,
                    tombstoned: vec![11, 47],
                },
            ],
            folded: 1,
            accumulator: 5,
        };
        let bytes = encode_merge_state(&state);
        let read_back = decode_merge_state(&bytes).expect("a well formed merge state decodes");
        assert_eq!(read_back, state);
    }

    /// A merge state truncated inside its own header - before the input list
    /// even starts - is refused there, naming the merge state rather than
    /// the manifest it has not reached yet.
    #[test]
    fn a_merge_state_truncated_in_its_header_is_refused_not_panicked_on() {
        let state = MergeState {
            source_level: 0,
            target_level: 1,
            inputs: vec![SegmentMeta {
                id: 5,
                level: 0,
                covers_from: 0,
                covers_to: 1024,
                chunks: 1024,
                tombstoned: Vec::new(),
            }],
            folded: 1,
            accumulator: 5,
        };
        let bytes = encode_merge_state(&state);
        // The header is four `i64`s - keep fewer bytes than that.
        let truncated = &bytes[..10];
        let error = decode_merge_state(truncated).expect_err("truncated bytes must not decode");
        assert!(
            format!("{error:?}").contains("corrupt merge state"),
            "{error:?}"
        );
    }

    /// A merge state truncated inside its input list is refused there - the
    /// same "no unwrap, no indexing" guarantee, one level down.
    #[test]
    fn a_merge_state_truncated_in_its_inputs_is_refused_not_panicked_on() {
        let state = MergeState {
            source_level: 0,
            target_level: 1,
            inputs: vec![SegmentMeta {
                id: 5,
                level: 0,
                covers_from: 0,
                covers_to: 1024,
                chunks: 1024,
                tombstoned: Vec::new(),
            }],
            folded: 1,
            accumulator: 5,
        };
        let mut bytes = encode_merge_state(&state);
        bytes.truncate(bytes.len() - 3);
        let error = decode_merge_state(&bytes).expect_err("truncated bytes must not decode");
        assert!(
            format!("{error:?}").contains("corrupt segment manifest"),
            "{error:?}"
        );
    }

    /// A list of several in-flight merges - one for each of two different
    /// levels merging at once - round trips through its blob, each keeping
    /// its own inputs distinct from the other's.
    #[test]
    fn a_merge_state_list_round_trips_through_its_blob() {
        let level_zero = MergeState {
            source_level: 0,
            target_level: 1,
            inputs: vec![SegmentMeta {
                id: 5,
                level: 0,
                covers_from: 0,
                covers_to: 1024,
                chunks: 1024,
                tombstoned: Vec::new(),
            }],
            folded: 1,
            accumulator: 5,
        };
        let level_one = MergeState {
            source_level: 1,
            target_level: 2,
            inputs: vec![
                SegmentMeta {
                    id: 1,
                    level: 1,
                    covers_from: 0,
                    covers_to: 4096,
                    chunks: 4096,
                    tombstoned: vec![7],
                },
                SegmentMeta {
                    id: 2,
                    level: 1,
                    covers_from: 4096,
                    covers_to: 8192,
                    chunks: 3800,
                    tombstoned: Vec::new(),
                },
            ],
            folded: 2,
            accumulator: 20,
        };
        let states = vec![level_zero.clone(), level_one.clone()];
        let bytes = encode_merge_states(&states);
        let read_back = decode_merge_states(&bytes).expect("a well formed list decodes");
        assert_eq!(read_back, vec![level_zero, level_one]);
    }

    /// An empty list decodes back to an empty list, not an error - a table
    /// with nothing in flight that still happened to have a `%_state` row
    /// written under an empty count.
    #[test]
    fn an_empty_merge_state_list_round_trips() {
        let bytes = encode_merge_states(&[]);
        assert_eq!(decode_merge_states(&bytes).expect("decodes"), Vec::new());
    }
}

#[cfg(test)]
mod fuzz_seeded {
    /// How many inputs the seeded sweep below reads.
    const CASES: usize = 20_000;

    /// Returns the next value of a deterministic generator.
    ///
    /// @param state - the generator's state, advanced in place
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// None of the three index decoders panics on arbitrary bytes.
    ///
    /// **These read the index out of the database file**, so their input is
    /// whatever is on disk - which includes a file somebody else could write
    /// to, and a file a crash left half written. The stable-toolchain twin of
    /// `fuzz/fuzz_targets/store.rs`.
    #[test]
    fn the_index_decoders_never_panic_on_arbitrary_bytes() {
        let mut state = 0x1932_0004_u64;
        let mut accepted = 0usize;
        for _ in 0..CASES {
            let length = (next(&mut state) % 128) as usize;
            let bytes: Vec<u8> = (0..length).map(|_| next(&mut state) as u8).collect();
            accepted += usize::from(super::decode_segments(&bytes).is_ok());
            accepted += usize::from(super::decode_merge_state(&bytes).is_ok());
            accepted += usize::from(super::decode_merge_states(&bytes).is_ok());
            // `decode_vector` answers a vector rather than a result: it reads
            // whatever whole floats the bytes hold and stops. What is asserted
            // is that it cannot claim more than the bytes could carry.
            let floats = super::decode_vector(&bytes);
            assert!(
                floats.len() <= bytes.len() / 4,
                "decode_vector read {} floats out of {} bytes",
                floats.len(),
                bytes.len()
            );
        }
        assert!(accepted <= CASES * 3);
    }
}
