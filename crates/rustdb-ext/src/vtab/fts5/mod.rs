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

use rustdb_base::{varint, DbResult};
use rustdb_value::Value;

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
    fn integrity(&mut self, context: &mut Context<'_>) -> DbResult<Option<String>> {
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

/// Returns the `%_data` row one term's doclist is in, making one if needed.
fn term_row(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    term: &[u8],
    create: bool,
) -> DbResult<Option<i64>> {
    let key = [Value::Integer(SEGMENT), Value::owned_blob(term)?];
    if let Some(row) = shadows.read_keyed(context, b"idx", &key, 3)? {
        if let Some(page) = row.get(2).and_then(Value::as_integer) {
            return Ok(Some(page));
        }
    }
    if !create {
        return Ok(None);
    }
    let next = shadows
        .max_rowid(context, b"data")?
        .max(FIRST_TERM_ROW.saturating_sub(1))
        .saturating_add(1);
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
fn unrecognized() -> rustdb_base::DbError {
    rustdb_base::DbError::primary(rustdb_base::PrimaryCode::Error)
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
        let Some(page) = term_row(context, &self.shadows, term, true)? else {
            return Ok(());
        };
        let mut entries = match self.shadows.read_row(context, b"data", page)? {
            Some(row) => row
                .get(1)
                .and_then(Value::as_blob)
                .map(|blob| decode_doclist(blob.raw()))
                .unwrap_or_default(),
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
        self.shadows.write_row(
            context,
            b"data",
            page,
            &[Value::Null, Value::owned_blob(&encoded)?],
        )
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
            let Some(page) = term_row(context, &self.shadows, term, false)? else {
                continue;
            };
            let Some(row) = self.shadows.read_row(context, b"data", page)? else {
                continue;
            };
            let Some(blob) = row.get(1).and_then(Value::as_blob) else {
                continue;
            };
            let mut entries = decode_doclist(blob.raw());
            entries.retain(|entry| entry.rowid != rowid);
            if entries.is_empty() {
                self.shadows.delete_row(context, b"data", page)?;
                self.shadows.delete_keyed(
                    context,
                    b"idx",
                    &[Value::Integer(SEGMENT), Value::owned_blob(term)?],
                )?;
                continue;
            }
            let encoded = encode_doclist(&entries);
            self.shadows.write_row(
                context,
                b"data",
                page,
                &[Value::Null, Value::owned_blob(&encoded)?],
            )?;
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
        Value::Real(number) => Some(rustdb_value::numeric::real_to_text(*number)),
        Value::Null => None,
    }
}

/// One row a query matched, with what it needs to be scored.
#[derive(Clone, Debug)]
struct MatchedRow {
    /// Which row.
    rowid: i64,
    /// The score `rank` reports, which is negative so that smaller is better.
    score: f64,
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
}

impl VirtualCursor for Fts5Cursor {
    /// Runs the query and collects every row it matched.
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.matched.clear();
        self.phrases.clear();
        self.at = 0;
        if plan.index_number == PLAN_ROWID {
            let Some(rowid) = plan.arguments.first().and_then(Value::as_integer) else {
                return Ok(());
            };
            if self.shadows.read_row(context, b"content", rowid)?.is_some() {
                self.rows.push(MatchedRow { rowid, score: 0.0 });
            }
            return Ok(());
        }
        if plan.index_number & PLAN_MATCH == 0 {
            self.shadows.scan(context, b"content", |rowid, _| {
                self.rows.push(MatchedRow { rowid, score: 0.0 });
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
        let (rows, hits) = expr::evaluate(&query, context, &self.shadows, self.columns)?;
        let scores = bm25::score(
            &rows,
            &hits,
            &query,
            context,
            &self.shadows,
            &totals,
            self.columns,
        )?;
        self.phrases = query.phrases.clone();
        self.totals = totals;
        self.matched = hits;
        self.rows = scores
            .into_iter()
            .map(|(rowid, score)| MatchedRow { rowid, score })
            .collect();
        if plan.index_number & PLAN_RANKED != 0 {
            self.rows.sort_by(|left, right| {
                left.score
                    .partial_cmp(&right.score)
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
            return Ok(Value::Real(row.score));
        }
        if index as i32 == self.match_column {
            // The hidden column that carries the query is NULL when it is read
            // as a value; it exists to be *constrained*, not to be selected.
            return Ok(Value::Null);
        }
        let Some(content) = self.shadows.read_row(context, b"content", row.rowid)? else {
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
