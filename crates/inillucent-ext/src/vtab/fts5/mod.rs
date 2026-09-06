//! FTS5: a full-text index whose whole state is five shadow tables.
//!
//! Invariant: what is compared is the *answers*. `MATCH` finds the same rows in
//! the same order and `bm25()` returns the same numbers as the pinned release,
//! and there are differential tests for both - because that is what an
//! application depends on and it is what the parity contract can honestly
//! claim.
//!
//! The index inside `%_data` is **not** SQLite's, and that is recorded rather
//! than hidden. FTS5's segment format - the structure record, the doclist
//! encoding, the prefix-compressed segment b-trees, the automerge state - is
//! described only in comments inside `fts5_index.c` and is explicitly not a
//! published format; the R-Tree's is published and this engine matches it byte
//! for byte, which is the difference. What *is* matched here is everything a
//! reader outside the module can see: the five table names, and the layouts of
//! `%_content`, `%_docsize` and `%_config`, which hold the rows themselves.
//!
//! The representation, which is what the rest of this module is about:
//!
//! - `%_config(k, v)` holds the options, `version` among them.
//! - `%_content(id, c0, c1, ...)` holds the row as it was inserted.
//! - `%_docsize(id, sz)` holds one varint per column: how many tokens it had.
//!   `bm25` needs it and nothing else does.
//! - `%_data(id, block)` holds row 1, the totals - the document count and the
//!   token count per column - and one row per term, holding that term's whole
//!   doclist.
//! - `%_idx(segid, term, pgno)` is the term dictionary: `segid` is always zero,
//!   `term` is the term's bytes, and `pgno` is the `%_data` row that holds its
//!   doclist. Reading a term is therefore one seek and one row.
//!
//! A doclist is a run of entries, each: the rowid as a delta from the previous
//! one, then per column that has a position, the column number, how many
//! positions, and the positions as deltas. Everything is a varint, which is
//! what makes a doclist for a common word small enough to be worth keeping in
//! one row.

pub mod bm25;
pub mod expr;
pub mod tokenize;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use inillucent_base::{varint, DbResult};
use inillucent_value::Value;

use super::{
    constraint, failure, Change, ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan,
    IndexQuery, Module, ModuleArguments, ShadowTable, VirtualCursor, VirtualTable, ROWID_COLUMN,
};
use crate::shadow::ShadowTables;

use self::expr::Phrase;
use self::expr::Query;
use self::tokenize::Tokenizer;

/// The `%_data` row that holds the totals.
const TOTALS: i64 = 1;
/// The first `%_data` row a term's doclist may use.
const FIRST_TERM_ROW: i64 = 16;
/// The segment number the term dictionary uses.
///
/// SQLite's `%_idx` keys a term by the segment it is in; this index has one
/// logical segment, so every term is in segment zero and the key is really the
/// term. Keeping the column means the table's declaration is the one SQLite
/// writes, and a reader looking at the schema sees what it expects.
const SEGMENT: i64 = 0;

/// The FTS5 module.
pub struct Fts5Module;

/// One column of an FTS5 table, and what was written about it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ColumnSpec {
    /// The column name.
    name: Vec<u8>,
    /// Whether `UNINDEXED` was written, so the column is stored and not indexed.
    unindexed: bool,
}

/// What a `CREATE VIRTUAL TABLE ... USING fts5(...)` said.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Options {
    /// The indexed and stored columns, in order.
    columns: Vec<ColumnSpec>,
    /// The tokenizer's name and its arguments.
    tokenizer: Vec<Vec<u8>>,
}

/// Reads the arguments of a `CREATE VIRTUAL TABLE ... USING fts5(...)`.
///
/// An argument is either a column - a bare name, optionally followed by
/// `UNINDEXED` - or an option, written `name = value`. That is FTS5's own
/// grammar, and the reason a column cannot be called `tokenize`.
fn parse_options(arguments: &[Vec<u8>]) -> DbResult<Options> {
    let mut columns = Vec::new();
    let mut tokenizer = vec![b"unicode61".to_vec()];
    for argument in arguments {
        let text = String::from_utf8_lossy(argument).trim().to_string();
        if let Some((name, value)) = split_option(&text) {
            match name.to_ascii_lowercase().as_str() {
                "tokenize" => tokenizer = tokenize::parse_specification(&value),
                // The options this build understands and the ones it does not
                // are both accepted, because refusing one would make a schema
                // SQLite wrote unreadable. What is not understood is recorded
                // in `%_config` and changes nothing.
                _ => {}
            }
            continue;
        }
        // A column may be written `"a b" UNINDEXED`, so the words are split
        // with the quotes honoured rather than on whitespace - or the column
        // would be called `"a`.
        let words = tokenize::split_words(&text);
        let Some(name) = words.first() else {
            continue;
        };
        let unindexed = words
            .iter()
            .skip(1)
            .any(|word| word.eq_ignore_ascii_case(b"UNINDEXED"));
        columns.push(ColumnSpec {
            name: name.clone(),
            unindexed,
        });
    }
    if columns.is_empty() {
        return Err(failure("an fts5 table needs at least one column"));
    }
    Ok(Options { columns, tokenizer })
}

/// Splits `name = value`, which is how an option is written.
fn split_option(text: &str) -> Option<(String, String)> {
    let (name, value) = text.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

impl Module for Fts5Module {
    /// Returns the module's name.
    fn name(&self) -> &str {
        "fts5"
    }

    /// The five shadow tables the index lives in.
    fn shadow_tables(&self, arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        let options = parse_options(&arguments.arguments)?;
        let mut content = String::from("CREATE TABLE \"%_content\"(id INTEGER PRIMARY KEY");
        for index in 0..options.columns.len() {
            content.push_str(&format!(", c{index}"));
        }
        content.push(')');
        Ok(vec![
            ShadowTable {
                suffix: b"data".to_vec(),
                create_sql: "CREATE TABLE \"%_data\"(id INTEGER PRIMARY KEY, block BLOB)"
                    .to_string(),
            },
            ShadowTable {
                suffix: b"idx".to_vec(),
                create_sql: "CREATE TABLE \"%_idx\"(segid, term, pgno, PRIMARY KEY(segid, term)) \
                             WITHOUT ROWID"
                    .to_string(),
            },
            ShadowTable {
                suffix: b"content".to_vec(),
                create_sql: content,
            },
            ShadowTable {
                suffix: b"docsize".to_vec(),
                create_sql: "CREATE TABLE \"%_docsize\"(id INTEGER PRIMARY KEY, sz BLOB)"
                    .to_string(),
            },
            ShadowTable {
                suffix: b"config".to_vec(),
                create_sql: "CREATE TABLE \"%_config\"(k PRIMARY KEY, v) WITHOUT ROWID".to_string(),
            },
        ])
    }

    /// Connects to a table, writing its configuration when it is being created.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        let options = parse_options(&arguments.arguments)?;
        let mut columns: Vec<DeclaredColumn> = options
            .columns
            .iter()
            .map(|column| DeclaredColumn::visible(&String::from_utf8_lossy(&column.name)))
            .collect();
        // The table's own name is a hidden column, which is what makes
        // `t MATCH 'x'` a constraint on a column rather than a special form.
        columns.push(DeclaredColumn::hidden(&String::from_utf8_lossy(
            &arguments.table,
        )));
        columns.push(DeclaredColumn::hidden("rank"));
        Ok(Box::new(Fts5Table {
            tokenizer: Tokenizer::named(&options.tokenizer),
            options,
            shadows: ShadowTables::of(
                arguments,
                &[b"data", b"idx", b"content", b"docsize", b"config"],
            )?,
            declaration: Declaration {
                columns,
                without_rowid: false,
            },
            creating,
            pending: Buffer::default(),
        }))
    }
}

/// The plan number for a scan of every row.
const PLAN_SCAN: i32 = 0;
/// The plan number for a lookup by rowid.
const PLAN_ROWID: i32 = 1;
/// The plan number for a full-text match.
const PLAN_MATCH: i32 = 2;
/// The plan bit that says the rows come back ranked.
const PLAN_RANKED: i32 = 4;

/// One connected FTS5 table.
struct Fts5Table {
    options: Options,
    tokenizer: Tokenizer,
    shadows: ShadowTables,
    declaration: Declaration,
    creating: bool,
    /// The doclists this transaction has changed, shared with its cursors.
    pending: Buffer,
}

impl Fts5Table {
    /// Returns which declared column is the table's own hidden one.
    fn match_column(&self) -> i32 {
        self.options.columns.len() as i32
    }

    /// Returns which declared column is `rank`.
    fn rank_column(&self) -> i32 {
        self.match_column().saturating_add(1)
    }
}

impl VirtualTable for Fts5Table {
    /// Returns the indexed columns, then the table's name, then `rank`.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Chooses between a match, a rowid lookup, and a scan.
    ///
    /// A `MATCH` is *omitted* from the residual, because the engine cannot
    /// evaluate one: the operator means nothing outside the module, and a
    /// module that claimed it without applying it would return every row. It is
    /// the one constraint here that the module promises absolutely.
    fn best_index(&self, query: &mut IndexQuery) -> DbResult<()> {
        let mut plan = PLAN_SCAN;
        for index in 0..query.constraints.len() {
            let Some(constraint) = query.constraints.get(index).copied() else {
                continue;
            };
            if !constraint.usable {
                continue;
            }
            if constraint.op == ConstraintOp::Match && constraint.column == self.match_column() {
                query.use_constraint(index, true);
                plan = PLAN_MATCH;
                break;
            }
            if constraint.op == ConstraintOp::Eq && constraint.column == ROWID_COLUMN {
                query.use_constraint(index, true);
                query.index_number = PLAN_ROWID;
                query.estimated_cost = 1.0;
                query.estimated_rows = 1;
                return Ok(());
            }
        }
        // `ORDER BY rank` is the reason the column exists: a match already
        // knows every row's score, so ordering by it costs a sort of the
        // matches rather than a sort of the table.
        if plan == PLAN_MATCH {
            if let Some(order) = query.order_by.first() {
                if query.order_by.len() == 1
                    && order.column == self.rank_column()
                    && !order.descending
                {
                    query.ordered = true;
                    plan |= PLAN_RANKED;
                }
            }
        }
        query.index_number = plan;
        query.estimated_cost = if plan & PLAN_MATCH != 0 { 10.0 } else { 1.0e6 };
        query.estimated_rows = if plan & PLAN_MATCH != 0 { 10 } else { 1000 };
        Ok(())
    }

    /// Opens a cursor.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(Fts5Cursor {
            matched: Vec::new(),
            phrases: Vec::new(),
            query: None,
            hits_built: false,
            pending: Arc::clone(&self.pending),
            held: None,
            totals: Totals::empty(self.options.columns.len()),
            columns: self.options.columns.len(),
            names: self
                .options
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            match_column: self.match_column(),
            rank_column: self.rank_column(),
            tokenizer: self.tokenizer.clone(),
            shadows: self.shadows.clone(),
            rows: Vec::new(),
            at: 0,
            pattern: Vec::new(),
        }))
    }

    /// Writes the configuration the first time the table is created.
    fn begin(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        if !self.creating {
            return Ok(());
        }
        self.creating = false;
        self.shadows.write_keyed(
            context,
            b"config",
            1,
            &[Value::owned_text(b"version")?, Value::Integer(4)],
        )?;
        // `version` is the only row FTS5 writes. The tokenizer is *not* kept
        // here: it is re-read from the module arguments in `sqlite_master`
        // every time the table is connected, and a `tokenize` row would be a
        // row an application reading `%_config` would find and SQLite would
        // not have written.
        put_totals(
            context,
            &self.shadows,
            &Totals::empty(self.options.columns.len()),
        )
    }

    /// Applies one insert, update or delete.
    fn update(&mut self, context: &mut Context<'_>, change: &Change) -> DbResult<Option<i64>> {
        match change {
            Change::Delete(rowid) => {
                let Some(rowid) = rowid.as_integer() else {
                    return Ok(None);
                };
                self.remove(context, rowid)?;
                Ok(None)
            }
            Change::Insert { rowid, values } => {
                // A value in the table's own hidden column makes the statement
                // a command rather than a row.
                if let Some(command) = values
                    .get(self.match_column() as usize)
                    .filter(|value| !matches!(value, Value::Null))
                    .and_then(text_of)
                {
                    let argument = values.get(self.rank_column() as usize).cloned();
                    self.command(context, &command, argument)?;
                    return Ok(None);
                }
                let key = match rowid.as_integer() {
                    Some(key) => key,
                    None => self
                        .shadows
                        .max_rowid(context, b"content")?
                        .saturating_add(1),
                };
                if self.shadows.read_row(context, b"content", key)?.is_some() {
                    return Err(constraint(
                        "UNIQUE constraint failed: the rowid is already in the index",
                    ));
                }
                self.add(context, key, values)?;
                Ok(Some(key))
            }
            Change::Update {
                old_rowid,
                new_rowid,
                values,
            } => {
                let Some(old) = old_rowid.as_integer() else {
                    return Ok(None);
                };
                let new = new_rowid.as_integer().unwrap_or(old);
                self.remove(context, old)?;
                self.add(context, new, values)?;
                Ok(Some(new))
            }
        }
    }

    /// Checks that the index and the content agree.
    ///
    /// Every row in `%_content` has a size row, and every doclist entry names a
    /// row that is there. An index that has drifted from its content is the
    /// failure mode that matters: it answers queries with rows that are gone
    /// and misses rows that are present, and neither is visible from a query.
    /// Writes the doclists this transaction staged.
    ///
    /// Both engines call this before they commit, which is what makes the
    /// buffer a buffer: nothing durable is deferred past the transaction that
    /// wrote it, and a reader inside the transaction sees it because the
    /// cursors share the same handle.
    fn sync(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        flush_doclists(context, &self.shadows, &self.pending)
    }

    fn integrity(&mut self, context: &mut Context<'_>) -> DbResult<Option<String>> {
        // The check reads `%_data` rows rather than doclists, so it is the one
        // reader the buffer cannot answer: written out first.
        flush_doclists(context, &self.shadows, &self.pending)?;
        let mut problems = Vec::new();
        let mut rows = Vec::new();
        self.shadows.scan(context, b"content", |rowid, _| {
            rows.push(rowid);
            Ok(true)
        })?;
        for rowid in &rows {
            if self
                .shadows
                .read_row(context, b"docsize", *rowid)?
                .is_none()
            {
                problems.push(format!("row {rowid} has no size"));
            }
        }
        let mut terms = Vec::new();
        self.shadows.scan_keyed(context, b"idx", 2, |values| {
            let term = values
                .get(1)
                .and_then(Value::as_blob)
                .map(|blob| blob.raw().to_vec());
            let page = values.get(2).and_then(Value::as_integer);
            if let (Some(term), Some(page)) = (term, page) {
                terms.push((term, page));
            }
            Ok(true)
        })?;
        for (term, page) in &terms {
            let Some(row) = self.shadows.read_row(context, b"data", *page)? else {
                problems.push(format!(
                    "the term {} has no doclist",
                    String::from_utf8_lossy(term)
                ));
                continue;
            };
            let Some(blob) = row.get(1).and_then(Value::as_blob) else {
                continue;
            };
            for entry in decode_doclist(blob.raw()) {
                if !rows.contains(&entry.rowid) {
                    problems.push(format!(
                        "the term {} names row {} which is not in the content",
                        String::from_utf8_lossy(term),
                        entry.rowid
                    ));
                }
            }
        }
        if problems.is_empty() {
            return Ok(None);
        }
        problems.truncate(20);
        Ok(Some(problems.join("; ")))
    }
}

/// The totals `bm25` needs: how many rows, and how many tokens per column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Totals {
    /// How many rows the index holds.
    pub rows: i64,
    /// How many tokens each column holds across every row.
    pub tokens: Vec<i64>,
}

impl Totals {
    /// Returns the totals of an empty index.
    fn empty(columns: usize) -> Totals {
        Totals {
            rows: 0,
            tokens: vec![0; columns],
        }
    }

    /// Returns the totals as the bytes `%_data` row one holds.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint(&mut out, self.rows as u64);
        write_varint(&mut out, self.tokens.len() as u64);
        for count in &self.tokens {
            write_varint(&mut out, *count as u64);
        }
        out
    }

    /// Reads the totals back.
    fn decode(bytes: &[u8]) -> Totals {
        let mut at = 0usize;
        let rows = read_varint(bytes, &mut at) as i64;
        let columns = read_varint(bytes, &mut at) as usize;
        let mut tokens = Vec::with_capacity(columns);
        for _ in 0..columns.min(1024) {
            tokens.push(read_varint(bytes, &mut at) as i64);
        }
        Totals { rows, tokens }
    }
}

/// Reads one varint, advancing the offset.
fn read_varint(bytes: &[u8], at: &mut usize) -> u64 {
    let Some(rest) = bytes.get(*at..) else {
        return 0;
    };
    let Ok(decoded) = varint::decode(rest) else {
        *at = bytes.len();
        return 0;
    };
    *at = at.saturating_add(decoded.len);
    decoded.value
}

/// Appends one varint.
fn write_varint(out: &mut Vec<u8>, value: u64) {
    let mut buffer = [0u8; 9];
    if let Ok(used) = varint::encode(&mut buffer, value) {
        out.extend_from_slice(buffer.get(..used).unwrap_or(&[]));
    }
}

/// One row's appearance in one term's doclist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocEntry {
    /// Which row.
    pub rowid: i64,
    /// The positions the term appears at, by column.
    pub columns: Vec<(usize, Vec<u32>)>,
}

impl DocEntry {
    /// Returns how many times the term appears in one column.
    pub fn count_in(&self, column: usize) -> usize {
        self.columns
            .iter()
            .find(|(index, _)| *index == column)
            .map(|(_, positions)| positions.len())
            .unwrap_or(0)
    }

    /// Returns how many times the term appears anywhere in the row.
    pub fn count(&self) -> usize {
        self.columns
            .iter()
            .map(|(_, positions)| positions.len())
            .sum()
    }
}

/// Encodes a doclist.
fn encode_doclist(entries: &[DocEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = 0i64;
    for entry in entries {
        write_varint(&mut out, entry.rowid.wrapping_sub(previous) as u64);
        previous = entry.rowid;
        write_varint(&mut out, entry.columns.len() as u64);
        for (column, positions) in &entry.columns {
            write_varint(&mut out, *column as u64);
            write_varint(&mut out, positions.len() as u64);
            let mut last = 0u32;
            for position in positions {
                write_varint(&mut out, u64::from(position.wrapping_sub(last)));
                last = *position;
            }
        }
    }
    out
}

/// Returns the last rowid in an encoded doclist, without decoding it.
///
/// Walks the same structure `decode_doclist` walks and allocates nothing: it
/// keeps the running rowid and steps over each entry's varints. It answers
/// `None` unless the walk consumes the blob exactly, which is what makes it
/// safe to act on - a doclist this cannot account for byte-for-byte is one the
/// caller falls back to decoding, rather than one it appends to on a guess.
fn last_doclist_rowid(bytes: &[u8]) -> Option<i64> {
    let mut at = 0usize;
    let mut rowid = 0i64;
    let mut seen = false;
    while at < bytes.len() {
        rowid = rowid.wrapping_add(read_varint(bytes, &mut at) as i64);
        let columns = read_varint(bytes, &mut at) as usize;
        if columns > 4096 || at > bytes.len() {
            return None;
        }
        for _ in 0..columns {
            let _column = read_varint(bytes, &mut at);
            let count = read_varint(bytes, &mut at) as usize;
            if count > 1 << 24 || at > bytes.len() {
                return None;
            }
            for _ in 0..count {
                let _delta = read_varint(bytes, &mut at);
            }
            if at > bytes.len() {
                return None;
            }
        }
        seen = true;
    }
    if at == bytes.len() && seen {
        Some(rowid)
    } else {
        None
    }
}

/// Appends one entry to an encoded doclist, given its rowid delta.
///
/// The bytes it writes are exactly the bytes `encode_doclist` would write for
/// the same entry in the same position, which is the property that lets the
/// fast path below produce a doclist indistinguishable from a re-encoded one.
fn append_doclist_entry(out: &mut Vec<u8>, delta: i64, entry: &DocEntry) {
    write_varint(out, delta as u64);
    write_varint(out, entry.columns.len() as u64);
    for (column, positions) in &entry.columns {
        write_varint(out, *column as u64);
        write_varint(out, positions.len() as u64);
        let mut last = 0u32;
        for position in positions {
            write_varint(out, u64::from(position.wrapping_sub(last)));
            last = *position;
        }
    }
}

/// Decodes a doclist.
pub fn decode_doclist(bytes: &[u8]) -> Vec<DocEntry> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    let mut rowid = 0i64;
    while at < bytes.len() {
        rowid = rowid.wrapping_add(read_varint(bytes, &mut at) as i64);
        let columns = read_varint(bytes, &mut at) as usize;
        if columns > 4096 {
            break;
        }
        let mut per_column = Vec::with_capacity(columns);
        for _ in 0..columns {
            let column = read_varint(bytes, &mut at) as usize;
            let count = read_varint(bytes, &mut at) as usize;
            if count > 1 << 24 {
                return entries;
            }
            let mut positions = Vec::with_capacity(count.min(4096));
            let mut last = 0u32;
            for _ in 0..count {
                last = last.wrapping_add(read_varint(bytes, &mut at) as u32);
                positions.push(last);
            }
            per_column.push((column, positions));
        }
        entries.push(DocEntry {
            rowid,
            columns: per_column,
        });
        if at >= bytes.len() {
            break;
        }
    }
    entries
}

/// Collects the rows a doclist names, without decoding their positions.
///
/// A row is collected when it has a position in a column the caller asked for
/// and that the table declares - the same test [`expr::phrase_hits`] applies at
/// offset zero, decided by walking the varints rather than by building the
/// vectors that would prove it.
///
/// @param bytes - the doclist as `%_data` holds it
/// @param wanted - the column a `column:term` filter named, if any
/// @param columns - how many columns the table declares
/// @param out - where the rowids are appended, in doclist order
pub fn doclist_rows(bytes: &[u8], wanted: Option<usize>, columns: usize, out: &mut Vec<i64>) {
    let mut at = 0usize;
    let mut rowid = 0i64;
    while at < bytes.len() {
        rowid = rowid.wrapping_add(read_varint(bytes, &mut at) as i64);
        let count = read_varint(bytes, &mut at) as usize;
        if count > 4096 {
            break;
        }
        let mut matched = false;
        for _ in 0..count {
            let column = read_varint(bytes, &mut at) as usize;
            let positions = read_varint(bytes, &mut at) as usize;
            if positions > 1 << 24 {
                return;
            }
            for _ in 0..positions {
                let _ = read_varint(bytes, &mut at);
            }
            if positions == 0 || column >= columns {
                continue;
            }
            if wanted.is_some_and(|asked| asked != column) {
                continue;
            }
            matched = true;
        }
        if matched {
            out.push(rowid);
        }
        if at >= bytes.len() {
            break;
        }
    }
}

/// Reads the totals row.
fn get_totals(context: &mut Context<'_>, shadows: &ShadowTables, columns: usize) -> Totals {
    let Ok(Some(row)) = shadows.read_row(context, b"data", TOTALS) else {
        return Totals::empty(columns);
    };
    let Some(blob) = row.get(1).and_then(Value::as_blob) else {
        return Totals::empty(columns);
    };
    Totals::decode(blob.raw())
}

/// Writes the totals row.
fn put_totals(context: &mut Context<'_>, shadows: &ShadowTables, totals: &Totals) -> DbResult<()> {
    shadows.write_row(
        context,
        b"data",
        TOTALS,
        &[Value::Null, Value::owned_blob(&totals.encode())?],
    )
}

/// The doclists this transaction has changed but has not written yet.
///
/// **A doclist is rewritten once per transaction, not once per document.**
/// `merge_term` reads a term's whole doclist, appends one entry and writes the
/// whole thing back, so a term that appears in every document of a bulk load is
/// read and written once per document and the bytes moved grow with the load:
/// five hundred documents over a ten-word vocabulary moved about ten megabytes
/// to store thirty kilobytes. The entries are the same entries and the row is
/// the same row; only the number of times it is written changes.
///
/// It is shared with the cursors the table opens, so a query inside the same
/// transaction reads what the transaction has written - which is what makes
/// this a buffer rather than a delayed write.
#[derive(Default)]
pub struct Pending {
    /// The `%_data` page a doclist belongs in, and the doclist.
    doclists: BTreeMap<i64, Vec<u8>>,
    /// How many bytes the doclists hold, so the buffer can be bounded.
    bytes: usize,
    /// The highest `%_data` page handed out, staged rows included.
    ///
    /// A staged row is not in the table yet, so `max_rowid` cannot see it and
    /// two new terms in one transaction would be given the same page.
    highest: i64,
    /// Which `%_data` page each term's doclist is in, for the terms this
    /// transaction has looked up.
    ///
    /// **The dictionary is asked once per term, not once per occurrence.**
    /// Indexing a document looks up every token it holds, so a twelve-word
    /// document costs twelve keyed descents into `%_idx` and five hundred of
    /// them cost six thousand - over a dictionary of ten terms. Only found
    /// terms are cached: a term that is not there yet is created through this
    /// same function, which is what would otherwise have to invalidate a
    /// remembered absence.
    terms: BTreeMap<Vec<u8>, i64>,
}

/// A handle on the buffer, shared by a table and the cursors it opens.
pub type Buffer = Arc<Mutex<Pending>>;

/// How many bytes of doclists are held before the buffer is written out.
///
/// A bound rather than a tuning knob: without one, a bulk load of a large
/// corpus would hold the whole index in memory. Flushing early costs a rewrite
/// of what is held, which is what the unbuffered path paid per document.
const PENDING_BUDGET: usize = 8 * 1024 * 1024;

/// Returns a term's doclist, from the buffer when it is staged there.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged doclists
/// @param page - the `%_data` row the doclist lives in
fn read_doclist(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    page: i64,
) -> DbResult<Option<Vec<u8>>> {
    if let Ok(held) = buffer.lock() {
        if let Some(bytes) = held.doclists.get(&page) {
            return Ok(Some(bytes.clone()));
        }
    }
    Ok(shadows.read_row(context, b"data", page)?.and_then(|row| {
        row.get(1)
            .and_then(Value::as_blob)
            .map(|b| b.raw().to_vec())
    }))
}

/// Appends one entry to a staged doclist without copying it.
///
/// **The buffer is written into, not read out of and put back.** Reading a
/// staged doclist hands back a copy, and appending through that copy moves the
/// whole doclist per occurrence - which is the same quadratic the buffer was
/// added to remove, relocated from the file into memory. A term in five hundred
/// documents grew a two-kilobyte list, so the copies were half a megabyte per
/// term.
///
/// Answers false when the term is not staged, or when the entry does not sort
/// after everything already there; the caller then takes the general path.
///
/// @param buffer - the staged doclists
/// @param page - the `%_data` row the doclist belongs in
/// @param entry - the entry to append
fn append_staged(buffer: &Buffer, page: i64, entry: &DocEntry) -> bool {
    let Ok(mut held) = buffer.lock() else {
        return false;
    };
    let Some(bytes) = held.doclists.get_mut(&page) else {
        return false;
    };
    let Some(last) = last_doclist_rowid(bytes) else {
        return false;
    };
    if entry.rowid <= last {
        return false;
    }
    let was = bytes.len();
    append_doclist_entry(bytes, entry.rowid.wrapping_sub(last), entry);
    let grew = bytes.len().saturating_sub(was);
    held.bytes = held.bytes.saturating_add(grew);
    true
}

/// Stages a doclist to be written when the buffer is next flushed.
///
/// @param buffer - the staged doclists
/// @param page - the `%_data` row it belongs in
/// @param bytes - the whole doclist
fn stage_doclist(buffer: &Buffer, page: i64, bytes: Vec<u8>) {
    if let Ok(mut held) = buffer.lock() {
        let was = held.doclists.get(&page).map(Vec::len).unwrap_or(0);
        held.bytes = held.bytes.saturating_sub(was).saturating_add(bytes.len());
        held.doclists.insert(page, bytes);
    }
}

/// Forgets a staged doclist, for one whose row is being deleted.
///
/// @param buffer - the staged doclists
/// @param page - the `%_data` row
fn forget_doclist(buffer: &Buffer, page: i64) {
    if let Ok(mut held) = buffer.lock() {
        if let Some(bytes) = held.doclists.remove(&page) {
            held.bytes = held.bytes.saturating_sub(bytes.len());
        }
    }
}

/// Reports whether the buffer is holding more than it should.
///
/// @param buffer - the staged doclists
fn buffer_is_full(buffer: &Buffer) -> bool {
    buffer
        .lock()
        .map(|held| held.bytes > PENDING_BUDGET)
        .unwrap_or(false)
}

/// Writes every staged doclist and empties the buffer.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged doclists
fn flush_doclists(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
) -> DbResult<()> {
    let staged: Vec<(i64, Vec<u8>)> = match buffer.lock() {
        Ok(mut held) => {
            held.bytes = 0;
            core::mem::take(&mut held.doclists).into_iter().collect()
        }
        Err(_) => return Ok(()),
    };
    for (page, bytes) in staged {
        shadows.write_row(
            context,
            b"data",
            page,
            &[Value::Null, Value::owned_blob(&bytes)?],
        )?;
    }
    Ok(())
}

fn term_row(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    term: &[u8],
    create: bool,
) -> DbResult<Option<i64>> {
    if let Ok(held) = buffer.lock() {
        if let Some(page) = held.terms.get(term) {
            return Ok(Some(*page));
        }
    }
    let key = [Value::Integer(SEGMENT), Value::owned_blob(term)?];
    if let Some(row) = shadows.read_keyed(context, b"idx", &key, 3)? {
        if let Some(page) = row.get(2).and_then(Value::as_integer) {
            if let Ok(mut held) = buffer.lock() {
                held.terms.insert(term.to_vec(), page);
            }
            return Ok(Some(page));
        }
    }
    if !create {
        return Ok(None);
    }
    // The buffer's highest is consulted because a staged row is not in the
    // table yet: `max_rowid` cannot see it, and two new terms in one
    // transaction would otherwise be given the same page.
    let staged = buffer.lock().map(|held| held.highest).unwrap_or(0);
    let next = shadows
        .max_rowid(context, b"data")?
        .max(FIRST_TERM_ROW.saturating_sub(1))
        .max(staged)
        .saturating_add(1);
    if let Ok(mut held) = buffer.lock() {
        held.highest = held.highest.max(next);
        held.terms.insert(term.to_vec(), next);
    }
    shadows.write_keyed(
        context,
        b"idx",
        2,
        &[
            Value::Integer(SEGMENT),
            Value::owned_blob(term)?,
            Value::Integer(next),
        ],
    )?;
    Ok(Some(next))
}

/// Returns the error a command FTS5 does not know reports.
///
/// SQLite says nothing more than `SQL logic error` here - there is no message
/// naming the command - so neither does this.
fn unrecognized() -> inillucent_base::DbError {
    inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Error)
}

impl Fts5Table {
    /// Runs one of the module's special commands.
    ///
    /// `integrity-check` and `rebuild` do what they say. The merge family -
    /// `optimize`, `merge`, `automerge`, `crisismerge`, `usermerge`, `pgsz` -
    /// is accepted and does nothing, because this index keeps one doclist per
    /// term rather than a stack of segments to merge: there is no work for them
    /// to ask for. They are accepted rather than refused so that an application
    /// written against SQLite runs unchanged, and they are listed here rather
    /// than ignored silently so the next reader knows the difference is
    /// deliberate.
    fn command(
        &mut self,
        context: &mut Context<'_>,
        command: &[u8],
        argument: Option<Value<'static>>,
    ) -> DbResult<()> {
        let name = String::from_utf8_lossy(command).into_owned();
        let argument = argument.filter(|value| !matches!(value, Value::Null));
        match name.as_str() {
            "integrity-check" => match self.integrity(context)? {
                Some(problem) => Err(failure(problem)),
                None => Ok(()),
            },
            "rebuild" => self.rebuild(context),
            // There is nothing to merge or flush: one doclist per term is the
            // whole index, so the segment machinery these ask about does not
            // exist here. They are accepted rather than refused so that an
            // application written against SQLite runs unchanged.
            "optimize" | "flush" => Ok(()),
            "merge" => match argument {
                Some(_) => Ok(()),
                None => Err(unrecognized()),
            },
            // The settings *are* kept, because `%_config` is a table an
            // application reads. Nothing here acts on them - see above - but a
            // value that was written and cannot be read back would be a
            // difference a reader can see.
            "automerge" | "crisismerge" | "deletemerge" | "pgsz" | "rank" | "secure-delete"
            | "usermerge" => {
                let Some(value) = argument else {
                    return Err(unrecognized());
                };
                self.shadows.write_keyed(
                    context,
                    b"config",
                    1,
                    &[Value::owned_text(name.as_bytes())?, value],
                )
            }
            "delete-all" => Err(failure(
                "'delete-all' may only be used with a contentless or external content fts5 table",
            )),
            _ => Err(unrecognized()),
        }
    }

    /// Throws the index away and builds it again from the content.
    ///
    /// The content table is the truth: it holds the rows exactly as they were
    /// inserted, and everything else - the doclists, the term dictionary, the
    /// sizes, the totals - is derived from it. That is what makes a rebuild
    /// possible at all, and what makes it the repair for an index that has
    /// drifted.
    fn rebuild(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        // Every `%_data` row is about to go, staged ones included: a doclist
        // left in the buffer would be written back after the wipe.
        if let Ok(mut held) = self.pending.lock() {
            held.doclists.clear();
            held.bytes = 0;
            held.highest = 0;
            held.terms.clear();
        }
        let width = self.options.columns.len();
        let mut rows: Vec<(i64, Vec<Value<'static>>)> = Vec::new();
        self.shadows.scan(context, b"content", |rowid, values| {
            rows.push((rowid, values.iter().skip(1).take(width).cloned().collect()));
            Ok(true)
        })?;
        let mut doclists = Vec::new();
        self.shadows.scan(context, b"data", |rowid, _| {
            if rowid != TOTALS {
                doclists.push(rowid);
            }
            Ok(true)
        })?;
        for rowid in doclists {
            self.shadows.delete_row(context, b"data", rowid)?;
        }
        let mut terms = Vec::new();
        self.shadows.scan_keyed(context, b"idx", 2, |values| {
            if let Some(term) = values.get(1).and_then(Value::as_blob) {
                terms.push(term.raw().to_vec());
            }
            Ok(true)
        })?;
        for term in terms {
            self.shadows.delete_keyed(
                context,
                b"idx",
                &[Value::Integer(SEGMENT), Value::owned_blob(&term)?],
            )?;
        }
        let mut sizes = Vec::new();
        self.shadows.scan(context, b"docsize", |rowid, _| {
            sizes.push(rowid);
            Ok(true)
        })?;
        for rowid in sizes {
            self.shadows.delete_row(context, b"docsize", rowid)?;
        }
        put_totals(context, &self.shadows, &Totals::empty(width))?;
        for (rowid, values) in rows {
            self.add(context, rowid, &values)?;
        }
        Ok(())
    }

    /// Adds one row to the content and to the index.
    fn add(
        &mut self,
        context: &mut Context<'_>,
        rowid: i64,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let width = self.options.columns.len();
        let mut content = vec![Value::Null];
        for index in 0..width {
            content.push(values.get(index).cloned().unwrap_or(Value::Null));
        }
        self.shadows
            .write_row(context, b"content", rowid, &content)?;

        let mut sizes = vec![0i64; width];
        let mut postings: Vec<(Vec<u8>, usize, u32)> = Vec::new();
        for (index, column) in self.options.columns.iter().enumerate() {
            if column.unindexed {
                continue;
            }
            let Some(text) = values.get(index).and_then(text_of) else {
                continue;
            };
            for (position, token) in self.tokenizer.tokens(&text).into_iter().enumerate() {
                sizes[index] = sizes[index].saturating_add(1);
                postings.push((token, index, position as u32));
            }
        }
        let mut encoded = Vec::new();
        for size in &sizes {
            write_varint(&mut encoded, *size as u64);
        }
        self.shadows.write_row(
            context,
            b"docsize",
            rowid,
            &[Value::Null, Value::owned_blob(&encoded)?],
        )?;

        postings.sort();
        let mut at = 0usize;
        while at < postings.len() {
            let Some((term, _, _)) = postings.get(at).cloned() else {
                break;
            };
            let mut columns: Vec<(usize, Vec<u32>)> = Vec::new();
            while let Some((candidate, column, position)) = postings.get(at) {
                if candidate != &term {
                    break;
                }
                match columns.iter_mut().find(|(index, _)| index == column) {
                    Some((_, positions)) => positions.push(*position),
                    None => columns.push((*column, vec![*position])),
                }
                at = at.saturating_add(1);
            }
            self.merge_term(context, &term, DocEntry { rowid, columns })?;
        }

        let mut totals = get_totals(context, &self.shadows, width);
        totals.rows = totals.rows.saturating_add(1);
        totals.tokens.resize(width, 0);
        for (index, size) in sizes.iter().enumerate() {
            if let Some(total) = totals.tokens.get_mut(index) {
                *total = total.saturating_add(*size);
            }
        }
        put_totals(context, &self.shadows, &totals)
    }

    /// Adds one entry to a term's doclist, keeping it in rowid order.
    fn merge_term(&self, context: &mut Context<'_>, term: &[u8], entry: DocEntry) -> DbResult<()> {
        let Some(page) = term_row(context, &self.shadows, &self.pending, term, true)? else {
            return Ok(());
        };
        // The ordinary case of a bulk load: the term is already staged and the
        // document sorts after every other, so the entry is written straight
        // into the buffer.
        if append_staged(&self.pending, page, &entry) {
            if buffer_is_full(&self.pending) {
                flush_doclists(context, &self.shadows, &self.pending)?;
            }
            return Ok(());
        }
        let existing: Option<Vec<u8>> = read_doclist(context, &self.shadows, &self.pending, page)?;

        // The ordinary case is a new document whose rowid is above every one
        // already in this term's list, and it is worth its own path. Decoding
        // the whole doclist to insert at the end and then re-encoding it costs
        // time and allocation proportional to how many documents already
        // contain the term, so a bulk index build was quadratic: measured at
        // 100, 200, 400, 800 and 1,600 documents sharing a vocabulary, the cost
        // of one insert rose 1.00x, 1.36x, 2.03x, 3.32x, 6.24x, and the total
        // went from 19 ms to 1,925 ms for sixteen times the documents.
        //
        // Appending writes the bytes `encode_doclist` would have written for
        // the same entry, so the row is byte-for-byte the one the slow path
        // produces. It is taken only when the existing blob can be walked
        // exactly - anything else falls through and is decoded.
        if let Some(bytes) = existing.as_deref() {
            if let Some(last) = last_doclist_rowid(bytes) {
                if entry.rowid > last {
                    let mut out = Vec::with_capacity(bytes.len().saturating_add(16));
                    out.extend_from_slice(bytes);
                    append_doclist_entry(&mut out, entry.rowid.wrapping_sub(last), &entry);
                    stage_doclist(&self.pending, page, out);
                    if buffer_is_full(&self.pending) {
                        flush_doclists(context, &self.shadows, &self.pending)?;
                    }
                    return Ok(());
                }
            }
        }

        let mut entries = match existing.as_deref() {
            Some(bytes) => decode_doclist(bytes),
            None => Vec::new(),
        };
        match entries.binary_search_by_key(&entry.rowid, |existing| existing.rowid) {
            Ok(position) => {
                if let Some(slot) = entries.get_mut(position) {
                    *slot = entry;
                }
            }
            Err(position) => entries.insert(position, entry),
        }
        let encoded = encode_doclist(&entries);
        stage_doclist(&self.pending, page, encoded);
        if buffer_is_full(&self.pending) {
            flush_doclists(context, &self.shadows, &self.pending)?;
        }
        Ok(())
    }

    /// Removes one row from the content and from every doclist it is in.
    fn remove(&mut self, context: &mut Context<'_>, rowid: i64) -> DbResult<()> {
        let Some(content) = self.shadows.read_row(context, b"content", rowid)? else {
            return Ok(());
        };
        let width = self.options.columns.len();
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for (index, column) in self.options.columns.iter().enumerate() {
            if column.unindexed {
                continue;
            }
            let Some(text) = content.get(index.saturating_add(1)).and_then(text_of) else {
                continue;
            };
            terms.extend(self.tokenizer.tokens(&text));
        }
        terms.sort();
        terms.dedup();
        for term in &terms {
            let Some(page) = term_row(context, &self.shadows, &self.pending, term, false)? else {
                continue;
            };
            let Some(bytes) = read_doclist(context, &self.shadows, &self.pending, page)? else {
                continue;
            };
            let mut entries = decode_doclist(&bytes);
            entries.retain(|entry| entry.rowid != rowid);
            if entries.is_empty() {
                // Staged as well as stored: the row may exist only in the
                // buffer, and a delete that left it there would write it back.
                forget_doclist(&self.pending, page);
                if let Ok(mut held) = self.pending.lock() {
                    held.terms.remove(term.as_slice());
                }
                self.shadows.delete_row(context, b"data", page)?;
                self.shadows.delete_keyed(
                    context,
                    b"idx",
                    &[Value::Integer(SEGMENT), Value::owned_blob(term)?],
                )?;
                continue;
            }
            let encoded = encode_doclist(&entries);
            stage_doclist(&self.pending, page, encoded);
        }

        let sizes = match self.shadows.read_row(context, b"docsize", rowid)? {
            Some(row) => row
                .get(1)
                .and_then(Value::as_blob)
                .map(|blob| decode_sizes(blob.raw(), width))
                .unwrap_or_else(|| vec![0; width]),
            None => vec![0; width],
        };
        self.shadows.delete_row(context, b"docsize", rowid)?;
        self.shadows.delete_row(context, b"content", rowid)?;
        let mut totals = get_totals(context, &self.shadows, width);
        totals.rows = (totals.rows - 1).max(0);
        totals.tokens.resize(width, 0);
        for (index, size) in sizes.iter().enumerate() {
            if let Some(total) = totals.tokens.get_mut(index) {
                *total = (*total - size).max(0);
            }
        }
        put_totals(context, &self.shadows, &totals)
    }
}

/// Reads a `%_docsize` blob back into one count per column.
pub fn decode_sizes(bytes: &[u8], columns: usize) -> Vec<i64> {
    let mut at = 0usize;
    (0..columns)
        .map(|_| read_varint(bytes, &mut at) as i64)
        .collect()
}

/// Returns a value as the text the tokenizer reads.
fn text_of(value: &Value<'static>) -> Option<Vec<u8>> {
    match value {
        Value::Text(text) => Some(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => Some(blob.raw().to_vec()),
        Value::Integer(number) => Some(number.to_string().into_bytes()),
        Value::Real(number) => Some(inillucent_value::numeric::real_to_text(*number)),
        Value::Null => None,
    }
}

/// One row a query matched, with what it needs to be scored.
#[derive(Clone, Debug)]
struct MatchedRow {
    /// Which row.
    rowid: i64,
    /// The score `rank` reports, which is negative so that smaller is better.
    ///
    /// `None` until something asks for it. Scoring one row costs a read of its
    /// `%_docsize`, and a query that names neither `rank` nor `ORDER BY rank`
    /// never looks at the answer - which is most of them, and `count(*)` in
    /// particular. The one query that needs every score up front is the ranked
    /// one, where the score *is* the sort key.
    score: Option<f64>,
}

/// A cursor over the rows one query matched.
struct Fts5Cursor {
    columns: usize,
    /// The column names, for a `column:term` filter to resolve against.
    names: Vec<Vec<u8>>,
    match_column: i32,
    rank_column: i32,
    tokenizer: Tokenizer,
    shadows: ShadowTables,
    rows: Vec<MatchedRow>,
    at: usize,
    /// The query the rows came from, for the auxiliary functions.
    pattern: Vec<u8>,
    /// What each phrase matched, kept so `bm25(t, w1, w2)` can score the row
    /// again with the weights that call asked for. `rank` is the same score
    /// with every weight one, so the two cannot disagree.
    matched: Vec<expr::Hits>,
    /// The phrases, in the order `matched` holds them.
    phrases: Vec<Phrase>,
    /// The collection totals the score is relative to.
    totals: Totals,
    /// The parsed query, kept so the hits can be built if a score is asked for.
    query: Option<Query>,
    /// Whether `matched` holds this query's hits.
    ///
    /// A query that reads no score never builds them; one that reads a score
    /// after not building them builds them once, here, rather than per row.
    hits_built: bool,
    /// The doclists the transaction has staged, shared with the table.
    ///
    /// A query inside a transaction that has written has to see what it wrote,
    /// and the buffer is where those doclists are until the commit.
    pending: Buffer,
    /// The `%_content` row the cursor is on, kept for the columns after the
    /// first.
    ///
    /// **One read per row, not one per column.** `column` is asked for each
    /// column in turn and read the whole row back for every one of them, so a
    /// two-column table read `%_content` twice per matched row and a
    /// ten-column table ten times. The row is the same row each time.
    held: Option<(i64, Vec<Value<'static>>)>,
}

impl Fts5Cursor {
    /// Builds the phrase hits if the cheap walk did not.
    ///
    /// The one caller that needs them is a score, and a score is asked for per
    /// row - so this runs once and every later row reads what it left.
    ///
    /// @param context - the host
    fn ensure_hits(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        if self.hits_built {
            return Ok(());
        }
        self.hits_built = true;
        let Some(query) = self.query.clone() else {
            return Ok(());
        };
        let (_, hits) =
            expr::evaluate(&query, context, &self.shadows, &self.pending, self.columns)?;
        self.matched = hits;
        Ok(())
    }
}

impl VirtualCursor for Fts5Cursor {
    /// Runs the query and collects every row it matched.
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.matched.clear();
        self.phrases.clear();
        self.at = 0;
        self.held = None;
        if plan.index_number == PLAN_ROWID {
            let Some(rowid) = plan.arguments.first().and_then(Value::as_integer) else {
                return Ok(());
            };
            if self.shadows.read_row(context, b"content", rowid)?.is_some() {
                self.rows.push(MatchedRow { rowid, score: None });
            }
            return Ok(());
        }
        if plan.index_number & PLAN_MATCH == 0 {
            self.shadows.scan(context, b"content", |rowid, _| {
                self.rows.push(MatchedRow { rowid, score: None });
                Ok(true)
            })?;
            return Ok(());
        }
        let Some(pattern) = plan.arguments.first().and_then(text_of) else {
            return Ok(());
        };
        self.pattern = pattern.clone();
        let query = Query::parse(&pattern, &self.tokenizer, &self.names)?;
        let totals = get_totals(context, &self.shadows, self.columns);
        let ranked = plan.index_number & PLAN_RANKED != 0;
        // **A ranked plan needs the positions, and most plans do not.** The
        // cheap walk answers `None` for the queries whose answer depends on
        // them, and those fall through to the full evaluation.
        let cheap = if ranked {
            None
        } else {
            expr::evaluate_rows(&query, context, &self.shadows, &self.pending, self.columns)?
        };
        let (rows, hits) = match cheap {
            Some(rows) => (rows, Vec::new()),
            None => expr::evaluate(&query, context, &self.shadows, &self.pending, self.columns)?,
        };
        self.hits_built = !hits.is_empty();
        if ranked {
            // The sort key has to exist before the sort, so this is the one
            // plan that scores every row up front.
            let scores = bm25::score(
                &rows,
                &hits,
                &query,
                context,
                &self.shadows,
                &totals,
                self.columns,
            )?;
            self.rows = scores
                .into_iter()
                .map(|(rowid, score)| MatchedRow {
                    rowid,
                    score: Some(score),
                })
                .collect();
        } else {
            self.rows = rows
                .into_iter()
                .map(|rowid| MatchedRow { rowid, score: None })
                .collect();
        }
        self.phrases = query.phrases.clone();
        self.totals = totals;
        self.matched = hits;
        self.query = Some(query);
        if ranked {
            self.rows.sort_by(|left, right| {
                // Both sides are `Some` here: this arm is only reached when
                // `filter` scored every row, which it does for exactly this
                // plan. An unscored row sorts as zero rather than panicking.
                left.score
                    .unwrap_or(0.0)
                    .partial_cmp(&right.score.unwrap_or(0.0))
                    .unwrap_or(core::cmp::Ordering::Equal)
                    .then(left.rowid.cmp(&right.rowid))
            });
        } else {
            self.rows.sort_by_key(|row| row.rowid);
        }
        Ok(())
    }

    /// Moves to the next matching row.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        self.held = None;
        Ok(())
    }

    /// Returns whether the walk is finished.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column of the current row.
    fn column(&mut self, context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(Value::Null);
        };
        if index as i32 == self.rank_column {
            if let Some(score) = row.score {
                return Ok(Value::Real(score));
            }
            if self.phrases.is_empty() {
                // A row reached without a `MATCH` has nothing to score against,
                // which is what SQLite answers zero for.
                return Ok(Value::Real(0.0));
            }
            // The same arithmetic `bm25::score` would have done in `filter`,
            // for this row alone: `rank` *is* `bm25` with every weight one, so
            // the two cannot disagree.
            self.ensure_hits(context)?;
            let weights = vec![1.0f64; self.columns];
            let sizes = bm25::row_sizes(context, &self.shadows, row.rowid, self.columns)?;
            return Ok(Value::Real(bm25::score_row(
                row.rowid,
                &self.matched,
                &self.phrases,
                &self.totals,
                &sizes,
                &weights,
            )));
        }
        if index as i32 == self.match_column {
            // The hidden column that carries the query is NULL when it is read
            // as a value; it exists to be *constrained*, not to be selected.
            return Ok(Value::Null);
        }
        if self.held.as_ref().map(|(rowid, _)| *rowid) != Some(row.rowid) {
            self.held = self
                .shadows
                .read_row(context, b"content", row.rowid)?
                .map(|values| (row.rowid, values));
        }
        let Some((_, content)) = self.held.as_ref() else {
            return Ok(Value::Null);
        };
        Ok(content
            .get(index.saturating_add(1))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Returns the row's rowid.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.rows.get(self.at).map(|row| row.rowid).unwrap_or(0))
    }

    /// Answers `bm25(t [, weight...])` on the current row.
    ///
    /// The weights are per call, which is why this cannot be a column: the
    /// same row scores differently in `bm25(docs)` and `bm25(docs, 10.0, 1.0)`
    /// in the same SELECT. A row reached without a `MATCH` scores zero, which
    /// is what SQLite answers for a function that had no query to score
    /// against.
    fn auxiliary(
        &mut self,
        context: &mut Context<'_>,
        name: &[u8],
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        if name != b"bm25" {
            return Err(crate::vtab::failure(format!(
                "no such function: {}",
                String::from_utf8_lossy(name)
            )));
        }
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(Value::Null);
        };
        if self.phrases.is_empty() {
            return Ok(Value::Real(0.0));
        }
        let mut weights = vec![1.0f64; self.columns];
        for (index, argument) in arguments.iter().take(self.columns).enumerate() {
            weights[index] = argument.as_real().unwrap_or(1.0);
        }
        // The hits are the query's, so the same maps score every row of it.
        self.ensure_hits(context)?;
        let hits = self.matched.clone();
        let sizes = bm25::row_sizes(context, &self.shadows, row.rowid, self.columns)?;
        Ok(Value::Real(bm25::score_row(
            row.rowid,
            &hits,
            &self.phrases,
            &self.totals,
            &sizes,
            &weights,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A doclist round-trips, positions and all.
    #[test]
    fn a_doclist_round_trips() {
        let entries = vec![
            DocEntry {
                rowid: 1,
                columns: vec![(0, vec![0, 3, 9])],
            },
            DocEntry {
                rowid: 40,
                columns: vec![(0, vec![2]), (1, vec![0, 1])],
            },
        ];
        let encoded = encode_doclist(&entries);
        assert_eq!(decode_doclist(&encoded), entries);
    }

    /// The totals round-trip.
    #[test]
    fn the_totals_round_trip() {
        let totals = Totals {
            rows: 17,
            tokens: vec![100, 250],
        };
        assert_eq!(Totals::decode(&totals.encode()), totals);
    }

    /// A column list and the options are told apart by the equals sign.
    #[test]
    fn columns_and_options_are_told_apart() {
        let options = parse_options(&[
            b"title".to_vec(),
            b"body".to_vec(),
            b"meta UNINDEXED".to_vec(),
            b"tokenize = 'ascii'".to_vec(),
        ])
        .expect("parses");
        assert_eq!(options.columns.len(), 3);
        assert_eq!(options.columns[0].name, b"title");
        assert!(!options.columns[1].unindexed);
        assert!(options.columns[2].unindexed);
        assert_eq!(options.tokenizer, vec![b"ascii".to_vec()]);
    }

    /// A quoted column name keeps its spaces and loses its quotes.
    #[test]
    fn a_quoted_column_is_unquoted() {
        let options = parse_options(&[b"\"a b\"".to_vec()]).expect("parses");
        assert_eq!(options.columns[0].name, b"a b");
        let flagged = parse_options(&[b"\"a b\" UNINDEXED".to_vec()]).expect("parses");
        assert_eq!(flagged.columns[0].name, b"a b");
        assert!(flagged.columns[0].unindexed);
    }

    /// A table with no columns is refused.
    #[test]
    fn a_table_needs_a_column() {
        assert!(parse_options(&[b"tokenize = 'ascii'".to_vec()]).is_err());
    }

    /// A doclist entry counts its term per column and overall.
    #[test]
    fn an_entry_counts_per_column() {
        let entry = DocEntry {
            rowid: 1,
            columns: vec![(0, vec![1, 2]), (2, vec![5])],
        };
        assert_eq!(entry.count_in(0), 2);
        assert_eq!(entry.count_in(1), 0);
        assert_eq!(entry.count_in(2), 1);
        assert_eq!(entry.count(), 3);
    }
}
