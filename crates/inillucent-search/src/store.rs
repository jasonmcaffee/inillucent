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
    /// only a single-pass build removes the old one.
    pub const CHUNKS: &str = "chunks";
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
    pub fn deltas_above(&self, context: &mut Context<'_>, covered: i64) -> DbResult<Vec<Delta>> {
        let mut found = Vec::new();
        self.tables.scan(context, b"delta", |sequence, values| {
            if sequence > covered {
                found.push(Delta {
                    sequence,
                    commit: values.get(1).and_then(Value::as_integer).unwrap_or(0),
                    id: values.get(2).and_then(Value::as_integer).unwrap_or(0),
                    op: Op::from_code(values.get(3).and_then(Value::as_integer).unwrap_or(2)),
                    digest: values.get(4).and_then(Value::as_integer).unwrap_or(0),
                });
            }
            Ok(true)
        })?;
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

    /// Removes every generation below a number.
    ///
    /// Never called by a write, only by the explicit maintenance command. A
    /// generation is immutable and a reader may still be inside one, so
    /// reclaiming the space is a decision an application makes rather than a
    /// side effect of an insert.
    pub fn drop_generations_below(&self, context: &mut Context<'_>, keep: i64) -> DbResult<usize> {
        let mut doomed = Vec::new();
        self.tables.scan(context, b"gen", |rowid, values| {
            if values
                .get(1)
                .and_then(Value::as_integer)
                .is_some_and(|generation| generation < keep)
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
}
