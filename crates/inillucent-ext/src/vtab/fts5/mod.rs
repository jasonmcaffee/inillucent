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
pub mod vocab;

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

/// Which full-text surface a table presents.
///
/// **One index, two front ends.** FTS3, FTS4 and FTS5 are three generations of
/// the same idea and SQLite ships two separate implementations of it; here the
/// index - the tokenizer, the term dictionary, the doclists, the sizes and the
/// totals - is one thing, and the dialect decides what a table made with it
/// *looks* like: which columns it declares, what its auxiliary functions are
/// called, and in what order they take their arguments.
///
/// What that does **not** promise is SQLite's FTS3 file layout. This engine
/// does not write SQLite's file format at all, so a `%_segdir` written to match
/// one byte for byte would be a shape nothing reads; the shadow tables are this
/// index's own, under FTS5's names, and the surface above them is FTS3's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// `fts5`: `rank`, `bm25`, `highlight`, and `snippet(t, col, ...)`.
    Five,
    /// `fts3` and `fts4`: `docid`, `snippet(t, start, ...)`, `offsets`,
    /// `matchinfo`.
    Three,
}

/// The FTS5 module.
pub struct Fts5Module;

/// The FTS3 and FTS4 module.
pub struct Fts3Module {
    /// The name this registration answers to.
    name: &'static str,
}

impl Fts3Module {
    /// Returns the `fts3` registration.
    pub fn three() -> Fts3Module {
        Fts3Module { name: "fts3" }
    }

    /// Returns the `fts4` registration.
    pub fn four() -> Fts3Module {
        Fts3Module { name: "fts4" }
    }
}

impl Module for Fts3Module {
    /// Returns the module's name.
    fn name(&self) -> &str {
        self.name
    }

    /// The same shadow tables the FTS5 index uses.
    fn shadow_tables(&self, arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        Fts5Module.shadow_tables(arguments)
    }

    /// Connects, declaring the FTS3 surface over the FTS5 index.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        connect_with(arguments, creating, Dialect::Three)
    }
}

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
    /// The table the rows live in, when they are not this table's own.
    ///
    /// `content='c'` makes an **external content** table: the index is built
    /// over rows that belong to `c`, and `%_content` is not created at all. It
    /// is what an application uses when the documents already exist and a
    /// second copy of them would double the file.
    content: Option<Vec<u8>>,
}

/// Reads the arguments of a `CREATE VIRTUAL TABLE ... USING fts5(...)`.
///
/// An argument is either a column - a bare name, optionally followed by
/// `UNINDEXED` - or an option, written `name = value`. That is FTS5's own
/// grammar, and the reason a column cannot be called `tokenize`.
fn parse_options(arguments: &[Vec<u8>]) -> DbResult<Options> {
    let mut columns = Vec::new();
    let mut tokenizer = vec![b"unicode61".to_vec()];
    let mut content: Option<Vec<u8>> = None;
    for argument in arguments {
        let text = String::from_utf8_lossy(argument).trim().to_string();
        if let Some((name, value)) = split_option(&text) {
            match name.to_ascii_lowercase().as_str() {
                "tokenize" => tokenizer = tokenize::parse_specification(&value),
                // **An external content table names its rows' owner**, and
                // reaching them is the one thing the module contract does not
                // give a module for free. It is asked for explicitly, by name,
                // through `ShadowTable::owner` - the same grant `fts5vocab`
                // uses - so the reach stays a grant rather than a hole.
                //
                // `content=''` is a *contentless* table, which is a different
                // thing: it stores no rows on purpose.
                "content" if !unquote_option(&value).is_empty() => {
                    content = Some(unquote_option(&value).as_bytes().to_vec())
                }
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
    Ok(Options {
        columns,
        tokenizer,
        content,
    })
}

/// Strips the quotes an option's value is written inside.
///
/// FTS5 takes `content='c'`, `content="c"` and a bare `content=c` as the same
/// thing, and the difference between the empty value and a name is what decides
/// whether a table is contentless or external.
fn unquote_option(value: &str) -> &str {
    let text = value.trim();
    for quote in ['\'', '"', '`'] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return &text[1..text.len() - 1];
        }
    }
    text
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
        // The suffix an external table's rows are reached under is empty,
        // because the name is the owner's own rather than one derived from it.
        for index in 0..options.columns.len() {
            content.push_str(&format!(", c{index}"));
        }
        content.push(')');
        Ok(vec![
            ShadowTable {
                suffix: b"data".to_vec(),
                create_sql: "CREATE TABLE \"%_data\"(id INTEGER PRIMARY KEY, block BLOB)"
                    .to_string(),
                owner: None,
            },
            ShadowTable {
                suffix: b"idx".to_vec(),
                create_sql: "CREATE TABLE \"%_idx\"(segid, term, pgno, PRIMARY KEY(segid, term)) \
                             WITHOUT ROWID"
                    .to_string(),
                owner: None,
            },
            match &options.content {
                // **Named rather than made.** The rows already exist in
                // somebody else's table, and creating a second, empty
                // `%_content` beside them would be an index over nothing.
                Some(owner) => ShadowTable {
                    suffix: Vec::new(),
                    create_sql: String::new(),
                    owner: Some(owner.clone()),
                },
                None => ShadowTable {
                    suffix: b"content".to_vec(),
                    create_sql: content,
                    owner: None,
                },
            },
            ShadowTable {
                suffix: b"docsize".to_vec(),
                create_sql: "CREATE TABLE \"%_docsize\"(id INTEGER PRIMARY KEY, sz BLOB)"
                    .to_string(),
                owner: None,
            },
            ShadowTable {
                suffix: b"config".to_vec(),
                create_sql: "CREATE TABLE \"%_config\"(k PRIMARY KEY, v) WITHOUT ROWID".to_string(),
                owner: None,
            },
        ])
    }

    /// Connects to a table, writing its configuration when it is being created.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        connect_with(arguments, creating, Dialect::Five)
    }
}

/// Connects one full-text table, in whichever dialect it was made with.
///
/// @param arguments - the `CREATE VIRTUAL TABLE` arguments
/// @param creating - whether the table is being made rather than reopened
/// @param dialect - which surface to declare
fn connect_with(
    arguments: &ModuleArguments,
    creating: bool,
    dialect: Dialect,
) -> DbResult<Box<dyn VirtualTable>> {
    {
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
        // **`rank` in FTS5 and `docid` in FTS3.** They occupy the same slot
        // because they are the same kind of thing - a column that is not part
        // of the document - and keeping one layout means the cursor has one
        // rule for which index is which rather than two.
        columns.push(DeclaredColumn::hidden(match dialect {
            Dialect::Five => "rank",
            Dialect::Three => "docid",
        }));
        let suffix = content_suffix(&options);
        let shadows =
            ShadowTables::of(arguments, &[b"data", b"idx", suffix, b"docsize", b"config"])?;
        Ok(Box::new(Fts5Table {
            dialect,
            tokenizer: Tokenizer::named(&options.tokenizer),
            content: suffix.to_vec(),
            external: options.content.clone(),
            options,
            shadows,
            declaration: Declaration {
                columns,
                without_rowid: false,
            },
            creating,
            pending: Buffer::default(),
            offsets: Vec::new(),
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
    /// Which surface this table presents.
    dialect: Dialect,
    options: Options,
    tokenizer: Tokenizer,
    shadows: ShadowTables,
    declaration: Declaration,
    creating: bool,
    /// The doclists this transaction has changed, shared with its cursors.
    pending: Buffer,
    /// The shadow suffix the rows are reached under.
    ///
    /// `content` for a table that owns its rows and the empty name for one
    /// whose rows belong to somebody else, because a granted shadow is looked
    /// up under the owner's own name rather than under a derived one.
    content: Vec<u8>,
    /// The table the rows belong to, when they are not this table's.
    external: Option<Vec<u8>>,
    /// Where each declared column sits in a stored row.
    ///
    /// Filled the first time a row is read, because it is read out of the
    /// *catalog* and a module is only shown one while a statement is running.
    offsets: Vec<usize>,
}

/// Returns the shadow suffix a table's rows are reached under.
///
/// @param options - what the `CREATE VIRTUAL TABLE` said
fn content_suffix(options: &Options) -> &'static [u8] {
    match options.content {
        Some(_) => b"",
        None => b"content",
    }
}

/// Returns where each declared column sits in a stored content row.
///
/// A row read out of a shadow table arrives as one value per stored column,
/// the rowid first. `%_content` is written by this module as `(id, c0, c1...)`,
/// so a declared column is one past its own number; an **external** content
/// table is somebody else's, so the columns are found by *name* - which is
/// what SQLite does too, and is why an external table has to declare columns
/// the index can recognise. A name that is not there reads as NULL rather than
/// as the wrong column.
///
/// @param external - the owner's name, when the rows are not this table's
/// @param columns - the declared columns of the full-text table
/// @param catalog - the schema the statement was compiled against
fn content_offsets(
    external: Option<&[u8]>,
    columns: &[ColumnSpec],
    catalog: Option<&inillucent_sql::catalog_view::StaticCatalog>,
) -> Vec<usize> {
    let names: Vec<Vec<u8>> = columns.iter().map(|column| column.name.clone()).collect();
    content_offsets_named(external, &names, catalog)
}

/// The same, from the column names alone.
///
/// The cursor holds names rather than specifications, and resolving from a
/// third shape would be a third description of the same rule.
///
/// @param external - the owner's name, when the rows are not this table's
/// @param names - the declared column names of the full-text table
/// @param catalog - the schema the statement was compiled against
fn content_offsets_named(
    external: Option<&[u8]>,
    names: &[Vec<u8>],
    catalog: Option<&inillucent_sql::catalog_view::StaticCatalog>,
) -> Vec<usize> {
    let Some(owner) = external else {
        return (0..names.len()).map(|at| at.saturating_add(1)).collect();
    };
    let found = catalog.and_then(|catalog| catalog.table_named(&owner.to_ascii_lowercase()));
    names
        .iter()
        .map(|name| {
            let folded = name.to_ascii_lowercase();
            found
                .and_then(|table| {
                    table
                        .columns
                        .iter()
                        .position(|held| held.name.to_ascii_lowercase() == folded)
                })
                .unwrap_or(usize::MAX)
        })
        .collect()
}

impl Fts5Table {
    /// Returns where each declared column sits in a stored row, filling it in.
    ///
    /// @param context - the running statement
    fn offsets(&mut self, context: &Context<'_>) -> Vec<usize> {
        if self.offsets.is_empty() {
            self.offsets = content_offsets(
                self.external.as_deref(),
                &self.options.columns,
                context.catalog,
            );
        }
        self.offsets.clone()
    }

    /// Returns whether the rows belong to another table.
    fn borrows_rows(&self) -> bool {
        self.external.is_some()
    }

    /// Returns whether the index already holds a row.
    ///
    /// **`%_docsize` rather than the content**, because the content may belong
    /// to somebody else: an external content table's rows exist before they are
    /// indexed, so asking the owner would report a collision for every row that
    /// has not been indexed yet. The size row is written by the indexing and is
    /// therefore the record of what this index holds.
    ///
    /// @param context - the running statement
    /// @param rowid - the row to look for
    fn holds_row(&self, context: &mut Context<'_>, rowid: i64) -> DbResult<bool> {
        Ok(self.shadows.read_row(context, b"docsize", rowid)?.is_some())
    }

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
            // **`docid` is the rowid**, so a predicate on it is a rowid
            // lookup rather than a filter over a scan. It shares the `rank`
            // slot, which is why the dialect has to be checked and not just
            // the column number.
            let docid = self.dialect == Dialect::Three && constraint.column == self.rank_column();
            if constraint.op == ConstraintOp::Eq && (constraint.column == ROWID_COLUMN || docid) {
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
                    && self.dialect == Dialect::Five
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
            dialect: self.dialect,
            content: self.content.clone(),
            offsets: content_offsets(self.external.as_deref(), &self.options.columns, None),
            external: self.external.clone(),
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
                    Some(key) => {
                        // A rowid the caller named may collide, so it is asked
                        // about; and it moves the mark, so the next allocated
                        // one is above it.
                        if self.holds_row(context, key)? {
                            return Err(constraint(
                                "UNIQUE constraint failed: the rowid is already in the index",
                            ));
                        }
                        note_content_rowid(&self.pending, key);
                        key
                    }
                    // Allocated, so it is one past everything - which is both
                    // the number and the answer to whether it is taken. See
                    // `Pending::content_highest`.
                    None => {
                        let suffix = self.content.clone();
                        next_content_rowid(context, &self.shadows, &self.pending, &suffix)?
                    }
                };
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
    /// Throws away everything the abandoned transaction staged.
    ///
    /// **FTS5 buffers.** A document's doclists, its `%_idx` dictionary rows and
    /// the running totals are held in `pending` and written out at `sync`,
    /// which is what makes a bulk load one flush rather than one flush per
    /// document. The engine undoes the shadow *trees* when a transaction is
    /// abandoned, so the file comes back correct - but the buffer is this
    /// module's own memory and nothing was undoing it, so the connection went
    /// on answering out of a staging area belonging to a transaction that never
    /// happened. It read two rows where the file had one, and nought where the
    /// file had two, until the database was reopened.
    ///
    /// The totals go with the doclists. They are a count of what is in the
    /// index, and a count that survived the rows it counted would be wrong in
    /// the other direction.
    fn rollback(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        if let Ok(mut held) = self.pending.lock() {
            *held = Pending::default();
        }
        Ok(())
    }

    /// Rolls back to a savepoint.
    ///
    /// The same discard, and it is correct for the same reason it is correct
    /// for a whole transaction: the engine flushes every module when a
    /// savepoint is *taken*, so everything staged before the point is already
    /// in the trees and under the undo log, and what is left in the buffer
    /// belongs entirely to the part being abandoned.
    fn rollback_to(&mut self, context: &mut Context<'_>, _number: i32) -> DbResult<()> {
        self.rollback(context)
    }

    fn sync(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        // The rowid mark is the transaction's, not the connection's: a rowid a
        // later transaction's delete frees is reused, exactly as SQLite reuses
        // it, and only a mark that ends here can be.
        if let Ok(mut held) = self.pending.lock() {
            held.content_highest = None;
        }
        flush_doclists(context, &self.shadows, &self.pending)
    }

    fn integrity(&mut self, context: &mut Context<'_>) -> DbResult<Option<String>> {
        // The check reads `%_data` rows rather than doclists, so it is the one
        // reader the buffer cannot answer: written out first.
        flush_doclists(context, &self.shadows, &self.pending)?;
        let mut problems = Vec::new();
        let mut rows = Vec::new();
        let suffix = self.content.clone();
        self.shadows.scan(context, &suffix, |rowid, _| {
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

/// Where an FTS5 index build spends its time, in nanoseconds.
///
/// **Measured rather than reasoned about**, which is the rule Phase 3's Part E
/// states for the read families and which applies here: the build path could be
/// tokenisation, the per-row shadow write, the doclist merge or the same
/// allocation-per-row shape found earlier in the index build, and guessing
/// which has been wrong twice on this project.
///
/// It is kept the way `create_index`'s stage timing is kept - permanently, and
/// read by the gate beside the ratio - because a workload under a bar needs its
/// breakdown printed beside the number, not reconstructed by a later ticket
/// from a profile nobody kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildStages {
    /// How many rows were indexed.
    pub rows: u64,
    /// Writing the row into `%_content`.
    pub content: u128,
    /// Turning the row's text into tokens.
    pub tokenize: u128,
    /// Encoding the per-column token counts and writing `%_docsize`.
    pub docsize: u128,
    /// Sorting the postings and grouping them by term, without the merge.
    pub group: u128,
    /// Adding this document to a term's doclist, for a term this transaction
    /// has already seen.
    pub terms: u128,
    /// The same, for a term this transaction has not seen before: finding or
    /// creating its `%_idx` row, and reading its doclist back.
    pub new_terms: u128,
    /// How many terms took that path.
    pub new_term_count: u64,
    /// Looking a term up in the `%_idx` dictionary.
    pub dictionary_read: u128,
    /// Writing a new term's `%_idx` row.
    pub dictionary_write: u128,
    /// Reading, updating and staging the corpus totals `bm25` needs.
    pub totals: u128,
    /// Writing the staged doclists out to `%_data`.
    pub flush: u128,
}

thread_local! {
    /// Where the FTS5 index builds on this thread have spent their time.
    ///
    /// A thread-local rather than a field on the table, because the gate reads
    /// it *after* the statement is over and the module is behind the
    /// connection: the same arrangement `INDEX_STAGES` uses in the gate, one
    /// layer down.
    static BUILD_STAGES: std::cell::Cell<BuildStages> =
        const { std::cell::Cell::new(BuildStages {
            rows: 0,
            content: 0,
            tokenize: 0,
            docsize: 0,
            group: 0,
            terms: 0,
            new_terms: 0,
            new_term_count: 0,
            dictionary_read: 0,
            dictionary_write: 0,
            totals: 0,
            flush: 0,
        }) };
}

/// Adds one measurement to this thread's tally.
///
/// @param edit - what to add
fn record_stage(edit: impl FnOnce(&mut BuildStages)) {
    BUILD_STAGES.with(|held| {
        let mut stages = held.get();
        edit(&mut stages);
        held.set(stages);
    });
}

/// Returns where the FTS5 builds on this thread have spent their time.
pub fn build_stages() -> BuildStages {
    BUILD_STAGES.with(|held| held.get())
}

/// Forgets what this thread's FTS5 builds have spent, so the next is on its own.
pub fn reset_build_stages() {
    BUILD_STAGES.with(|held| held.set(BuildStages::default()));
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

/// Returns the totals, from the buffer when this transaction has them.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged rows
/// @param columns - how many columns the table has
fn buffered_totals(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    columns: usize,
) -> Totals {
    if let Ok(held) = buffer.lock() {
        if let Some(totals) = held.totals.as_ref() {
            return totals.clone();
        }
    }
    let totals = get_totals(context, shadows, columns);
    if let Ok(mut held) = buffer.lock() {
        held.totals = Some(totals.clone());
    }
    totals
}

/// Stages the totals, to be written when the transaction flushes.
///
/// @param buffer - the staged rows
/// @param totals - the totals as they now stand
fn stage_totals(buffer: &Buffer, totals: Totals) {
    if let Ok(mut held) = buffer.lock() {
        held.totals = Some(totals);
        held.totals_dirty = true;
    }
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
    doclists: BTreeMap<i64, Staged>,
    /// How many bytes the doclists hold, so the buffer can be bounded.
    bytes: usize,
    /// The highest `%_data` page handed out, staged rows included.
    ///
    /// A staged row is not in the table yet, so `max_rowid` cannot see it and
    /// two new terms in one transaction would be given the same page.
    highest: i64,
    /// The `%_idx` rows this transaction has made and not yet written.
    ///
    /// **A dictionary row per term, written in term order at the flush rather
    /// than one at a time as the terms arrive.** `%_idx` is an index tree keyed
    /// by the term, and the terms of a document arrive in whatever order the
    /// text put them in - so writing each as it appears is a descent and a
    /// possible split per term, into a tree that is growing under it. Held here
    /// and written in key order, the same run of terms is a walk to the right
    /// of the tree.
    ///
    /// It was measured before it was done, which is the rule this file's own
    /// history argues for: `extension.fts.build` spent 1.8 of its 9.4 ms
    /// writing 507 of these one at a time, against 0.5 ms reading them and
    /// 1.2 ms writing every doclist.
    ///
    /// A `BTreeMap` because the order it iterates in *is* the key order the
    /// write wants; sorting a vector at the flush would be the same thing done
    /// later and worse, since the map is also what a lookup needs.
    ///
    /// **Every reader that scans `%_idx` flushes first.** A staged row is
    /// invisible to a scan, and the scans are `expr.rs`'s prefix search,
    /// `integrity`, the delete-all command and `remove`.
    staged_terms: BTreeMap<Vec<u8>, i64>,
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
    /// The totals row, read once and written once per transaction.
    ///
    /// **One `%_data` row, and it was being rewritten per document.** `add`
    /// finishes by reading row 1, adding this document's counts and writing it
    /// back, so a bulk load of five hundred documents read and wrote the same
    /// row five hundred times - and each of those is a descent, a log record
    /// and a page image on a row that nothing reads until a query asks for a
    /// score. `fts.build` measured 0.10x against SQLite with 22.6 of its 23.7
    /// ms inside the module, and this is the part of it that is pure repetition.
    ///
    /// `None` means "not read yet this transaction", which is different from
    /// "there are no totals": an empty index has a totals row of zeroes.
    totals: Option<Totals>,
    /// Whether the buffered totals differ from what `%_data` holds.
    totals_dirty: bool,
    /// The highest `%_content` rowid this transaction has handed out.
    ///
    /// **Two tree descents per document, for a number the module already
    /// knows.** An insert that names no rowid asked `max_rowid` for one - a
    /// descent to the rightmost leaf - and then asked `read_row` whether that
    /// rowid was taken, which is a second descent for a question whose answer
    /// is no by construction. Together they were half the descents a document
    /// costs, and `fts.build` is five hundred documents in one transaction.
    ///
    /// It is cleared at `sync`, which is the transaction boundary, so a rowid
    /// freed by a delete in a *later* transaction is reused exactly as SQLite
    /// reuses it. Within one transaction the number only rises, which is the
    /// same rule a rowid table follows for rows it has just written.
    content_highest: Option<i64>,
}

/// One staged doclist, and the rowid it currently ends at.
///
/// **The rowid is remembered because finding it costs a walk of the whole
/// list.** A doclist is a run of varints with no length prefix and no index, so
/// the only way to read the last entry is to decode every entry before it -
/// and an append has to know it, because entries are stored as deltas.
///
/// That made appending O(the list so far), and a bulk load appends to the same
/// term once per document: five hundred documents over a ten-word vocabulary
/// walked about a hundred and twenty thousand entries to add five thousand.
/// `merge_term`'s own doc comment records the identical shape being fixed for
/// the *unbuffered* path, by appending instead of decoding; the buffered path
/// it introduced brought the walk back one level down.
#[derive(Default)]
struct Staged {
    /// The doclist as `%_data` will hold it.
    bytes: Vec<u8>,
    /// The rowid the last entry names, when there is one.
    last: Option<i64>,
}

/// A handle on the buffer, shared by a table and the cursors it opens.
pub type Buffer = Arc<Mutex<Pending>>;

/// How many bytes of doclists are held before the buffer is written out.
///
/// A bound rather than a tuning knob: without one, a bulk load of a large
/// corpus would hold the whole index in memory. Flushing early costs a rewrite
/// of what is held, which is what the unbuffered path paid per document.
const PENDING_BUDGET: usize = 8 * 1024 * 1024;

/// Returns the rowid an insert that named none is given.
///
/// The table is asked once per transaction and the mark rises from there.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged rows
/// @param suffix - the shadow the rows are reached under
fn next_content_rowid(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    suffix: &[u8],
) -> DbResult<i64> {
    if let Ok(mut held) = buffer.lock() {
        if let Some(highest) = held.content_highest {
            let next = highest.saturating_add(1);
            held.content_highest = Some(next);
            return Ok(next);
        }
    }
    let next = shadows.max_rowid(context, suffix)?.saturating_add(1);
    if let Ok(mut held) = buffer.lock() {
        held.content_highest = Some(next);
    }
    Ok(next)
}

/// Records that a rowid the caller named is now in the table.
///
/// @param buffer - the staged rows
/// @param rowid - the rowid that was written
fn note_content_rowid(buffer: &Buffer, rowid: i64) {
    if let Ok(mut held) = buffer.lock() {
        held.content_highest = Some(held.content_highest.unwrap_or(0).max(rowid));
    }
}

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
        if let Some(staged) = held.doclists.get(&page) {
            return Ok(Some(staged.bytes.clone()));
        }
    }
    Ok(shadows.read_row(context, b"data", page)?.and_then(|row| {
        row.get(1)
            .and_then(Value::as_blob)
            .map(|b| b.raw().to_vec())
    }))
}

/// What [`append_in_one_lock`] managed.
enum Appended {
    /// The entry is in the buffer, and whether the buffer is now over budget.
    Done {
        /// Whether the caller should flush.
        full: bool,
    },
    /// Nothing was written; the caller takes the general path.
    No,
}

/// Appends one document's occurrences of one term, under a single lock.
///
/// **The whole of the bulk-load path, in one critical section.** It answers
/// [`Appended::No`] and writes nothing whenever any of its three conditions
/// does not hold - the term has not been looked up in this transaction, its
/// doclist is not staged, or the document does not sort after everything
/// already in it - and the caller then does the general thing, which is the
/// same code that ran before this existed.
///
/// The postings are written straight out of the sorted run, so nothing is
/// allocated to describe them: they arrive sorted by column and then by
/// position, which is exactly the order a doclist entry holds.
///
/// @param buffer - the staged doclists
/// @param term - the term
/// @param rowid - the document
/// @param run - this term's postings in this document
fn append_in_one_lock(
    buffer: &Buffer,
    term: &[u8],
    rowid: i64,
    run: &[(Vec<u8>, usize, u32)],
) -> Appended {
    let Ok(mut held) = buffer.lock() else {
        return Appended::No;
    };
    let Some(page) = held.terms.get(term).copied() else {
        return Appended::No;
    };
    let Some(staged) = held.doclists.get_mut(&page) else {
        return Appended::No;
    };
    let Some(last) = staged.last else {
        return Appended::No;
    };
    if rowid <= last {
        return Appended::No;
    }
    let was = staged.bytes.len();
    append_run(&mut staged.bytes, rowid.wrapping_sub(last), run);
    staged.last = Some(rowid);
    let grew = staged.bytes.len().saturating_sub(was);
    held.bytes = held.bytes.saturating_add(grew);
    Appended::Done {
        full: held.bytes > PENDING_BUDGET,
    }
}

/// Writes one doclist entry straight out of a sorted run of postings.
///
/// The bytes are the ones [`append_doclist_entry`] writes for the same
/// occurrences - the two are checked against each other by
/// `a_run_appends_the_bytes_the_entry_would`, because a doclist written two
/// ways is a format with two definitions.
///
/// @param out - the doclist being appended to
/// @param delta - this document's rowid minus the previous entry's
/// @param run - the postings, sorted by column then position
fn append_run(out: &mut Vec<u8>, delta: i64, run: &[(Vec<u8>, usize, u32)]) {
    write_varint(out, delta as u64);
    // How many distinct columns the run touches, counted without a collection:
    // it is sorted, so a column starts wherever it differs from the one before.
    let mut columns = 0u64;
    let mut previous: Option<usize> = None;
    for (_, column, _) in run {
        if previous != Some(*column) {
            columns = columns.saturating_add(1);
            previous = Some(*column);
        }
    }
    write_varint(out, columns);
    let mut at = 0usize;
    while at < run.len() {
        let Some((_, column, _)) = run.get(at) else {
            break;
        };
        let start = at;
        while matches!(run.get(at), Some((_, held, _)) if held == column) {
            at = at.saturating_add(1);
        }
        write_varint(out, *column as u64);
        write_varint(out, at.saturating_sub(start) as u64);
        let mut last = 0u32;
        for (_, _, position) in run.get(start..at).unwrap_or_default() {
            write_varint(out, u64::from(position.wrapping_sub(last)));
            last = *position;
        }
    }
}

/// Groups a sorted run of postings into the per-column form a `DocEntry` holds.
///
/// Only for the paths that need a whole entry - the first document to hold a
/// term, and one that does not sort last - so the allocation it costs is off
/// the bulk-load path.
///
/// @param run - the postings, sorted by column then position
fn columns_of(run: &[(Vec<u8>, usize, u32)]) -> Vec<(usize, Vec<u32>)> {
    let mut columns: Vec<(usize, Vec<u32>)> = Vec::new();
    for (_, column, position) in run {
        match columns.last_mut() {
            Some((held, positions)) if held == column => positions.push(*position),
            _ => columns.push((*column, vec![*position])),
        }
    }
    columns
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
    let Some(staged) = held.doclists.get_mut(&page) else {
        return false;
    };
    // The remembered end, rather than a walk to find it. See `Staged`.
    let Some(last) = staged.last else {
        return false;
    };
    if entry.rowid <= last {
        return false;
    }
    let was = staged.bytes.len();
    append_doclist_entry(&mut staged.bytes, entry.rowid.wrapping_sub(last), entry);
    staged.last = Some(entry.rowid);
    let grew = staged.bytes.len().saturating_sub(was);
    held.bytes = held.bytes.saturating_add(grew);
    true
}

/// Stages a doclist to be written when the buffer is next flushed.
///
/// @param buffer - the staged doclists
/// @param page - the `%_data` row it belongs in
/// @param bytes - the whole doclist
fn stage_doclist(buffer: &Buffer, page: i64, bytes: Vec<u8>) {
    // The walk happens here, once per whole-list rewrite, rather than on every
    // append: this is the path a list reaches when it is read back or built
    // from scratch, and it is the only place the end is not already known.
    let last = last_doclist_rowid(&bytes);
    if let Ok(mut held) = buffer.lock() {
        let was = held
            .doclists
            .get(&page)
            .map(|staged| staged.bytes.len())
            .unwrap_or(0);
        held.bytes = held.bytes.saturating_sub(was).saturating_add(bytes.len());
        held.doclists.insert(page, Staged { bytes, last });
    }
}

/// Forgets a staged doclist, for one whose row is being deleted.
///
/// @param buffer - the staged doclists
/// @param page - the `%_data` row
fn forget_doclist(buffer: &Buffer, page: i64) {
    if let Ok(mut held) = buffer.lock() {
        if let Some(staged) = held.doclists.remove(&page) {
            held.bytes = held.bytes.saturating_sub(staged.bytes.len());
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
    let started = std::time::Instant::now();
    let answer = flush_doclists_timed(context, shadows, buffer);
    let spent = started.elapsed().as_nanos();
    record_stage(|stages| stages.flush = stages.flush.saturating_add(spent));
    answer
}

/// Writes every staged doclist and empties the buffer, without the timing.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged doclists
fn flush_doclists_timed(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
) -> DbResult<()> {
    // **The dictionary first, in term order.** `%_idx` is keyed by the term,
    // so taking the `BTreeMap` in its own order writes the run as a walk to the
    // right of the tree rather than a descent per term into a growing one.
    let terms: Vec<(Vec<u8>, i64)> = match buffer.lock() {
        Ok(mut held) => core::mem::take(&mut held.staged_terms)
            .into_iter()
            .collect(),
        Err(_) => return Ok(()),
    };
    let started = std::time::Instant::now();
    for (term, page) in terms {
        shadows.write_keyed(
            context,
            b"idx",
            2,
            &[
                Value::Integer(SEGMENT),
                Value::owned_blob(&term)?,
                Value::Integer(page),
            ],
        )?;
    }
    let spent = started.elapsed().as_nanos();
    record_stage(|stages| stages.dictionary_write = stages.dictionary_write.saturating_add(spent));
    let staged: Vec<(i64, Staged)> = match buffer.lock() {
        Ok(mut held) => {
            held.bytes = 0;
            core::mem::take(&mut held.doclists).into_iter().collect()
        }
        Err(_) => return Ok(()),
    };
    for (page, doclist) in staged {
        shadows.write_row(
            context,
            b"data",
            page,
            &[Value::Null, Value::owned_blob(&doclist.bytes)?],
        )?;
    }
    // The totals go out with them, once, rather than once per document. The
    // buffered copy is kept: a reader inside the same transaction has to see
    // what the transaction wrote, which is what makes this a buffer.
    let totals = match buffer.lock() {
        Ok(mut held) if held.totals_dirty => {
            held.totals_dirty = false;
            held.totals.clone()
        }
        _ => None,
    };
    if let Some(totals) = totals {
        put_totals(context, shadows, &totals)?;
    }
    Ok(())
}

fn term_row(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    term: &[u8],
    create: bool,
) -> DbResult<Option<Term>> {
    if let Ok(held) = buffer.lock() {
        if let Some(page) = held.terms.get(term) {
            return Ok(Some(Term {
                page: *page,
                fresh: false,
            }));
        }
    }
    let probed = std::time::Instant::now();
    let key = [Value::Integer(SEGMENT), Value::owned_blob(term)?];
    let found = shadows.read_keyed(context, b"idx", &key, 3)?;
    let probe_ns = probed.elapsed().as_nanos();
    record_stage(|stages| stages.dictionary_read = stages.dictionary_read.saturating_add(probe_ns));
    if let Some(row) = found {
        if let Some(page) = row.get(2).and_then(Value::as_integer) {
            if let Ok(mut held) = buffer.lock() {
                held.terms.insert(term.to_vec(), page);
            }
            return Ok(Some(Term { page, fresh: false }));
        }
    }
    if !create {
        return Ok(None);
    }
    // The buffer's highest is consulted because a staged row is not in the
    // table yet: `max_rowid` cannot see it, and two new terms in one
    // transaction would otherwise be given the same page.
    //
    // **And once it holds one, the table is not asked again.** `max_rowid` is a
    // descent per new term, and after the first the buffer's mark is already at
    // or above the table's: every page it has handed out is above every page in
    // the table, and a flush writes them at exactly those numbers. Asking
    // anyway was a tree walk per term of a bulk load's vocabulary - 500 of them
    // in `extension.fts.build`, which is 500 unique tokens.
    let staged = buffer.lock().map(|held| held.highest).unwrap_or(0);
    let floor = if staged > 0 {
        staged
    } else {
        shadows
            .max_rowid(context, b"data")?
            .max(FIRST_TERM_ROW.saturating_sub(1))
    };
    let next = floor.saturating_add(1);
    if let Ok(mut held) = buffer.lock() {
        held.highest = held.highest.max(next);
        held.terms.insert(term.to_vec(), next);
        // Written at the flush, in term order. See `Pending::staged_terms`.
        held.staged_terms.insert(term.to_vec(), next);
    }
    Ok(Some(Term {
        page: next,
        fresh: true,
    }))
}

/// A term's `%_data` page, and whether this call is what created it.
///
/// **`fresh` is what lets the caller skip a read it knows will miss.** A term
/// whose dictionary row was written a moment ago has no doclist, and looking
/// for one is a descent per new term - which on a bulk load is a descent per
/// word of the vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Term {
    /// The `%_data` row its doclist lives in.
    page: i64,
    /// Whether the row was created by the call that returned this.
    fresh: bool,
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
        let offsets = self.offsets(context);
        let mut rows: Vec<(i64, Vec<Value<'static>>)> = Vec::new();
        let suffix = self.content.clone();
        self.shadows.scan(context, &suffix, |rowid, values| {
            rows.push((
                rowid,
                offsets
                    .iter()
                    .map(|at| values.get(*at).cloned().unwrap_or(Value::Null))
                    .collect(),
            ));
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
        // The staged dictionary rows go out first, so the scan below sees
        // every term there is - a delete-all that missed one would leave it in
        // `%_idx` naming a `%_data` row that has been removed.
        flush_doclists(context, &self.shadows, &self.pending)?;
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
        let started = std::time::Instant::now();
        // **Nothing is stored when the rows are somebody else's.** An
        // external content table's rows are written through the owner, and a
        // copy written here would be a second, diverging one.
        if !self.borrows_rows() {
            let mut content = vec![Value::Null];
            for index in 0..width {
                content.push(values.get(index).cloned().unwrap_or(Value::Null));
            }
            self.shadows
                .write_row(context, b"content", rowid, &content)?;
        }
        let content_ns = started.elapsed().as_nanos();

        let started = std::time::Instant::now();
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
                if let Some(size) = sizes.get_mut(index) {
                    *size = size.saturating_add(1);
                }
                postings.push((token, index, position as u32));
            }
        }
        let tokenize_ns = started.elapsed().as_nanos();

        let started = std::time::Instant::now();
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
        let docsize_ns = started.elapsed().as_nanos();

        let started = std::time::Instant::now();
        let mut terms_ns = 0u128;
        postings.sort_unstable();
        let mut at = 0usize;
        while at < postings.len() {
            // **The run of postings for one term, as a slice rather than a
            // copy.** They are sorted by term, then column, then position, so
            // one term's occurrences are contiguous and already in the order a
            // doclist entry writes them - which is what lets the append below
            // read them in place. Building a `DocEntry` here allocated a
            // `Vec<(usize, Vec<u32>)>` per term per document, and a term that
            // occurs once is two allocations to describe one number.
            let start = at;
            let Some((term, _, _)) = postings.get(at) else {
                break;
            };
            while matches!(postings.get(at), Some((candidate, _, _)) if candidate == term) {
                at = at.saturating_add(1);
            }
            let merged = std::time::Instant::now();
            let run = postings.get(start..at).unwrap_or_default();
            self.merge_postings(context, rowid, run)?;
            terms_ns = terms_ns.saturating_add(merged.elapsed().as_nanos());
        }
        let group_ns = started.elapsed().as_nanos().saturating_sub(terms_ns);

        let started = std::time::Instant::now();
        let mut totals = buffered_totals(context, &self.shadows, &self.pending, width);
        totals.rows = totals.rows.saturating_add(1);
        totals.tokens.resize(width, 0);
        for (index, size) in sizes.iter().enumerate() {
            if let Some(total) = totals.tokens.get_mut(index) {
                *total = total.saturating_add(*size);
            }
        }
        stage_totals(&self.pending, totals);
        let totals_ns = started.elapsed().as_nanos();

        record_stage(|stages| {
            stages.rows = stages.rows.saturating_add(1);
            stages.content = stages.content.saturating_add(content_ns);
            stages.tokenize = stages.tokenize.saturating_add(tokenize_ns);
            stages.docsize = stages.docsize.saturating_add(docsize_ns);
            stages.group = stages.group.saturating_add(group_ns);
            stages.terms = stages.terms.saturating_add(terms_ns);
            stages.totals = stages.totals.saturating_add(totals_ns);
        });
        Ok(())
    }

    /// Adds one document's occurrences of one term to that term's doclist.
    ///
    /// **The bulk-load path takes one lock and allocates nothing.** It used to
    /// take three - `term_row`, `append_staged` and `buffer_is_full` each
    /// locked the buffer - and build a `Vec<(usize, Vec<u32>)>` to describe the
    /// occurrences before writing them out as varints. Neither is much on its
    /// own and both are per term per document: a five-hundred-document build
    /// over a ten-term vocabulary is fifteen thousand lock/unlock pairs and ten
    /// thousand allocations, and `extension.fts.build` spent 3.5 of its 8.7 ms
    /// in here.
    ///
    /// Everything the fast path needs is in the buffer, so it is one critical
    /// section: find the term's page, append the entry to the staged doclist,
    /// and read back whether the buffer is now full. The flush - which needs
    /// the host and cannot be done under the lock - happens after it.
    ///
    /// @param context - the host
    /// @param rowid - the document being indexed
    /// @param run - this term's postings, sorted by column then position
    fn merge_postings(
        &self,
        context: &mut Context<'_>,
        rowid: i64,
        run: &[(Vec<u8>, usize, u32)],
    ) -> DbResult<()> {
        let Some((term, _, _)) = run.first() else {
            return Ok(());
        };
        match append_in_one_lock(&self.pending, term, rowid, run) {
            Appended::Done { full } => {
                if full {
                    flush_doclists(context, &self.shadows, &self.pending)?;
                }
                return Ok(());
            }
            Appended::No => {}
        }
        let term = term.clone();
        let entry = DocEntry {
            rowid,
            columns: columns_of(run),
        };
        let started = std::time::Instant::now();
        let answer = self.merge_term(context, &term, entry);
        let spent = started.elapsed().as_nanos();
        record_stage(|stages| {
            stages.new_terms = stages.new_terms.saturating_add(spent);
            stages.new_term_count = stages.new_term_count.saturating_add(1);
        });
        answer
    }

    /// Adds one entry to a term's doclist, keeping it in rowid order.
    fn merge_term(&self, context: &mut Context<'_>, term: &[u8], entry: DocEntry) -> DbResult<()> {
        let Some(held) = term_row(context, &self.shadows, &self.pending, term, true)? else {
            return Ok(());
        };
        let page = held.page;
        // The ordinary case of a bulk load: the term is already staged and the
        // document sorts after every other, so the entry is written straight
        // into the buffer.
        if append_staged(&self.pending, page, &entry) {
            if buffer_is_full(&self.pending) {
                flush_doclists(context, &self.shadows, &self.pending)?;
            }
            return Ok(());
        }
        // A term whose dictionary row was created a moment ago has no doclist,
        // and asking for one is a descent that is certain to miss.
        let existing: Option<Vec<u8>> = if held.fresh {
            None
        } else {
            read_doclist(context, &self.shadows, &self.pending, page)?
        };

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
        let offsets = self.offsets(context);
        let suffix = self.content.clone();
        let Some(content) = self.shadows.read_row(context, &suffix, rowid)? else {
            return Ok(());
        };
        let width = self.options.columns.len();
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for (index, column) in self.options.columns.iter().enumerate() {
            if column.unindexed {
                continue;
            }
            let Some(text) = offsets
                .get(index)
                .and_then(|at| content.get(*at))
                .and_then(text_of)
            else {
                continue;
            };
            terms.extend(self.tokenizer.tokens(&text));
        }
        terms.sort();
        terms.dedup();
        for term in &terms {
            let Some(page) =
                term_row(context, &self.shadows, &self.pending, term, false)?.map(|held| held.page)
            else {
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
                // A term whose dictionary row is only staged is dropped
                // from the buffer rather than deleted from a table that does
                // not hold it yet - a staged row a delete did not see would be
                // written at the flush and resurrect the term.
                let was_staged = match self.pending.lock() {
                    Ok(mut held) => held.staged_terms.remove(term.as_slice()).is_some(),
                    Err(_) => false,
                };
                self.shadows.delete_row(context, b"data", page)?;
                if !was_staged {
                    self.shadows.delete_keyed(
                        context,
                        b"idx",
                        &[Value::Integer(SEGMENT), Value::owned_blob(term)?],
                    )?;
                }
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
        if !self.borrows_rows() {
            self.shadows.delete_row(context, b"content", rowid)?;
        }
        let mut totals = buffered_totals(context, &self.shadows, &self.pending, width);
        totals.rows = (totals.rows - 1).max(0);
        totals.tokens.resize(width, 0);
        for (index, size) in sizes.iter().enumerate() {
            if let Some(total) = totals.tokens.get_mut(index) {
                *total = (*total - size).max(0);
            }
        }
        stage_totals(&self.pending, totals);
        Ok(())
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
    /// Which surface this table presents.
    dialect: Dialect,
    columns: usize,
    /// The shadow suffix the rows are reached under.
    content: Vec<u8>,
    /// Where each declared column sits in a stored row.
    offsets: Vec<usize>,
    /// The table the rows belong to, when they are not this table's.
    external: Option<Vec<u8>>,
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

impl Fts5Cursor {
    /// Answers `highlight(t, column, open, close)` on the current row.
    ///
    /// The column's text is **re-tokenised here** rather than read out of the
    /// index, and that is the design rather than a shortcut. The index stores a
    /// token's *position* - which word it is - and highlighting needs its
    /// *extent* in bytes, which the index never held: a token is folded before
    /// it is stored, and folding changes its length. Re-tokenising is the only
    /// way to get from a position back to a range of the original text, and it
    /// is what SQLite does too.
    ///
    /// @param context - the statement's context
    /// @param arguments - the column index, the opening mark, the closing mark
    fn highlight(
        &mut self,
        context: &mut Context<'_>,
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let column = arguments
            .first()
            .and_then(Value::as_integer)
            .unwrap_or(0)
            .max(0) as usize;
        let open = text_argument(arguments.get(1));
        let close = text_argument(arguments.get(2));
        let Some(text) = self.column_text(context, column)? else {
            return Ok(Value::Null);
        };
        let marked = self.marked_spans(&text);
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0usize;
        for (start, end) in marked {
            out.extend_from_slice(text.get(at..start).unwrap_or_default());
            out.extend_from_slice(&open);
            out.extend_from_slice(text.get(start..end).unwrap_or_default());
            out.extend_from_slice(&close);
            at = end;
        }
        out.extend_from_slice(text.get(at..).unwrap_or_default());
        Ok(Value::owned_text(&out).unwrap_or(Value::Null))
    }

    /// Answers `snippet(t, column, open, close, ellipsis, tokens)`.
    ///
    /// The window is the run of `tokens` tokens holding the most matched ones,
    /// earliest when several tie - which is SQLite's rule and the reason a
    /// snippet of a long document lands on the interesting part rather than on
    /// its first sentence. The ellipsis is written only where text was actually
    /// cut, so a snippet of a short column reads as the whole column.
    ///
    /// @param context - the statement's context
    /// @param arguments - column, open, close, ellipsis, token count
    fn snippet(
        &mut self,
        context: &mut Context<'_>,
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let column = arguments
            .first()
            .and_then(Value::as_integer)
            .unwrap_or(0)
            .max(0) as usize;
        let open = text_argument(arguments.get(1));
        let close = text_argument(arguments.get(2));
        let ellipsis = text_argument(arguments.get(3));
        let wanted = arguments
            .get(4)
            .and_then(Value::as_integer)
            .unwrap_or(15)
            .clamp(1, 64) as usize;
        let Some(text) = self.column_text(context, column)? else {
            return Ok(Value::Null);
        };
        let spans = self.tokenizer.spans(&text);
        if spans.is_empty() {
            return Ok(Value::owned_text(&text).unwrap_or(Value::Null));
        }
        let wanted_terms = self.wanted_terms();
        let hit: Vec<bool> = spans
            .iter()
            .map(|(token, _, _)| matches_a_term(token, &wanted_terms))
            .collect();
        // The best window, by how many matched tokens it holds.
        let mut best = 0usize;
        let mut best_score = usize::MAX;
        for start in 0..spans.len() {
            let end = start.saturating_add(wanted).min(spans.len());
            let score = hit
                .get(start..end)
                .map(|run| run.iter().filter(|held| **held).count())
                .unwrap_or(0);
            if best_score == usize::MAX || score > best_score {
                best_score = score;
                best = start;
            }
            if end == spans.len() {
                break;
            }
        }
        let end = best.saturating_add(wanted).min(spans.len());
        let from = spans.get(best).map(|(_, start, _)| *start).unwrap_or(0);
        let to = spans
            .get(end.saturating_sub(1))
            .map(|(_, _, stop)| *stop)
            .unwrap_or(text.len());
        let mut out = Vec::with_capacity(to.saturating_sub(from).saturating_add(16));
        if best > 0 {
            out.extend_from_slice(&ellipsis);
        }
        let mut at = from;
        for (index, (_, start, stop)) in spans.iter().enumerate() {
            if index < best || index >= end {
                continue;
            }
            if !hit.get(index).copied().unwrap_or(false) {
                continue;
            }
            out.extend_from_slice(text.get(at..*start).unwrap_or_default());
            out.extend_from_slice(&open);
            out.extend_from_slice(text.get(*start..*stop).unwrap_or_default());
            out.extend_from_slice(&close);
            at = *stop;
        }
        out.extend_from_slice(text.get(at..to).unwrap_or_default());
        if end < spans.len() {
            out.extend_from_slice(&ellipsis);
        }
        Ok(Value::owned_text(&out).unwrap_or(Value::Null))
    }

    /// Returns one column's stored text for the current row.
    ///
    /// @param context - the statement's context
    /// Fills in where each declared column sits, the first time a row is read.
    ///
    /// `open` is handed no catalog - a cursor is made before a statement runs -
    /// so an external content table's column positions cannot be resolved until
    /// here, where a context exists.
    ///
    /// @param context - the running statement
    fn resolve_offsets(&mut self, context: &Context<'_>) {
        if self.external.is_some() && self.offsets.contains(&usize::MAX) {
            self.offsets =
                content_offsets_named(self.external.as_deref(), &self.names, context.catalog);
        }
    }

    /// @param column - which declared column
    fn column_text(
        &mut self,
        context: &mut Context<'_>,
        column: usize,
    ) -> DbResult<Option<Vec<u8>>> {
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(None);
        };
        if self.held.as_ref().map(|(rowid, _)| *rowid) != Some(row.rowid) {
            self.resolve_offsets(context);
            self.held = self
                .shadows
                .read_row(context, &self.content.clone(), row.rowid)?
                .map(|values| (row.rowid, values));
        }
        let Some((_, content)) = self.held.as_ref() else {
            return Ok(None);
        };
        Ok(
            match self.offsets.get(column).and_then(|at| content.get(*at)) {
                Some(Value::Text(text)) => Some(text.utf8_bytes().into_owned()),
                Some(Value::Blob(blob)) => Some(blob.raw().to_vec()),
                _ => None,
            },
        )
    }

    /// Returns the byte ranges the query's phrases occupy in one column.
    ///
    /// **A phrase, not a token.** `MATCH '"quick brown"'` marks the two words
    /// as one range and `MATCH 'quick AND brown'` marks them as two, because
    /// the first is one phrase of two terms and the second is two phrases of
    /// one - and the reference draws exactly that distinction. Marking every
    /// matched token and merging adjacent ones gets the first case right and
    /// the second wrong.
    ///
    /// The ranges come back in order and non-overlapping: two phrases that
    /// claim the same word - `'quick OR "quick brown"'` - would otherwise
    /// produce nested marks and text repeated between them.
    ///
    /// @param text - the column's bytes
    fn marked_spans(&self, text: &[u8]) -> Vec<(usize, usize)> {
        let spans = self.tokenizer.spans(text);
        let mut marked: Vec<(usize, usize)> = Vec::new();
        for phrase in &self.phrases {
            if phrase.terms.is_empty() {
                continue;
            }
            let mut at = 0usize;
            while at.saturating_add(phrase.terms.len()) <= spans.len() {
                let fits = phrase.terms.iter().enumerate().all(|(offset, term)| {
                    spans
                        .get(at.saturating_add(offset))
                        .is_some_and(|(token, _, _)| {
                            matches_a_term(token, std::slice::from_ref(term))
                        })
                });
                if !fits {
                    at = at.saturating_add(1);
                    continue;
                }
                let from = spans.get(at).map(|(_, start, _)| *start).unwrap_or(0);
                let to = spans
                    .get(at.saturating_add(phrase.terms.len()).saturating_sub(1))
                    .map(|(_, _, end)| *end)
                    .unwrap_or(from);
                marked.push((from, to));
                at = at.saturating_add(phrase.terms.len());
            }
        }
        marked.sort_unstable();
        marked.dedup();
        // Overlaps go, keeping the first - which is the longest match starting
        // earliest once the list is sorted by start.
        let mut kept: Vec<(usize, usize)> = Vec::with_capacity(marked.len());
        for (from, to) in marked {
            match kept.last() {
                Some((_, last)) if from < *last => {}
                _ => kept.push((from, to)),
            }
        }
        kept
    }

    /// Returns every term the query asked for, across its phrases.
    /// Returns `offsets(t)`: where every hit in the row is, as four numbers.
    ///
    /// **Column, term, byte offset, byte length**, one quadruple per hit,
    /// separated by single spaces and in column-then-position order. It is
    /// FTS3's most direct answer - the caller gets the positions and does its
    /// own marking - and the term number is the *query's*, so a two-word query
    /// reports 0 for one word and 1 for the other however they interleave in
    /// the text.
    ///
    /// @param context - the running statement
    fn offsets(&mut self, context: &mut Context<'_>) -> DbResult<Value<'static>> {
        let wanted = self.wanted_terms();
        let mut out: Vec<String> = Vec::new();
        for column in 0..self.columns {
            let Some(text) = self.column_text(context, column)? else {
                continue;
            };
            for (token, start, stop) in self.tokenizer.spans(&text) {
                let Some(term) = wanted
                    .iter()
                    .position(|term| matches_a_term(&token, std::slice::from_ref(term)))
                else {
                    continue;
                };
                out.push(format!(
                    "{column} {term} {start} {}",
                    stop.saturating_sub(start)
                ));
            }
        }
        Value::owned_text(out.join(" ").as_bytes())
    }

    /// Returns `matchinfo(t, format)`: the counts a caller ranks with.
    ///
    /// A blob of native-endian 32-bit integers, one group per format letter,
    /// in the order the letters were written. `pcx` is the default and is what
    /// FTS3's own documentation builds its example ranking function from:
    ///
    /// | letter | what it contributes |
    /// |---|---|
    /// | `p` | how many phrases the query has |
    /// | `c` | how many columns the table has |
    /// | `x` | three numbers per phrase and column: hits in this row, hits in every row, and rows with at least one |
    /// | `n` | how many rows the table holds |
    /// | `a` | the average token count of each column |
    /// | `l` | this row's token count per column |
    ///
    /// @param context - the running statement
    /// @param arguments - the format string, when one was written
    fn matchinfo(
        &mut self,
        context: &mut Context<'_>,
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let format = match arguments.first() {
            Some(Value::Text(text)) => text.utf8_bytes().into_owned(),
            _ => b"pcx".to_vec(),
        };
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(Value::Null);
        };
        self.ensure_hits(context)?;
        let sizes = bm25::row_sizes(context, &self.shadows, row.rowid, self.columns)?;
        let mut out: Vec<u8> = Vec::new();
        let mut push = |number: i64| out.extend_from_slice(&(number as u32).to_ne_bytes());
        for letter in format {
            match letter {
                b'p' => push(self.phrases.len() as i64),
                b'c' => push(self.columns as i64),
                b'n' => push(self.totals.rows),
                b'a' => {
                    for column in 0..self.columns {
                        let total = self.totals.tokens.get(column).copied().unwrap_or(0);
                        let average = if self.totals.rows > 0 {
                            total / self.totals.rows
                        } else {
                            0
                        };
                        push(average);
                    }
                }
                b'l' => {
                    for column in 0..self.columns {
                        push(sizes.get(column).copied().unwrap_or(0));
                    }
                }
                b'x' => {
                    for at in 0..self.phrases.len() {
                        let hits = self.matched.get(at);
                        for column in 0..self.columns {
                            let (here, every, rows) = match hits {
                                Some(hits) => phrase_counts(hits, row.rowid, column),
                                None => (0, 0, 0),
                            };
                            push(here);
                            push(every);
                            push(rows);
                        }
                    }
                }
                _ => {}
            }
        }
        Value::owned_blob(&out)
    }

    /// Returns every term the query asked for, in order.
    fn wanted_terms(&self) -> Vec<expr::Term> {
        self.phrases
            .iter()
            .flat_map(|phrase| phrase.terms.iter().cloned())
            .collect()
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
            if self
                .shadows
                .read_row(context, &self.content.clone(), rowid)?
                .is_some()
            {
                self.rows.push(MatchedRow { rowid, score: None });
            }
            return Ok(());
        }
        if plan.index_number & PLAN_MATCH == 0 {
            // **The index's own rows, not the owner's.** An external content
            // table's owner may hold rows this index has never seen, and a
            // scan that returned them would answer with documents no query
            // could match. `%_docsize` has one row per indexed document.
            let suffix: &[u8] = if self.external.is_some() {
                b"docsize"
            } else {
                b"content"
            };
            self.shadows.scan(context, suffix, |rowid, _| {
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
        // **The buffered totals, not the row.** A score is computed from the
        // document count and the token totals, and those are staged rather than
        // written per document - so a query inside the same transaction that
        // read `%_data` directly would score against the state the transaction
        // started from. That is what a buffer has to be transparent about.
        let totals = buffered_totals(context, &self.shadows, &self.pending, self.columns);
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
            // In FTS3 this slot is `docid`, which is the rowid under another
            // name and is the way that dialect writes a row's identity.
            if self.dialect == Dialect::Three {
                return Ok(Value::Integer(row.rowid));
            }
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
            self.resolve_offsets(context);
            self.held = self
                .shadows
                .read_row(context, &self.content.clone(), row.rowid)?
                .map(|values| (row.rowid, values));
        }
        let Some((_, content)) = self.held.as_ref() else {
            return Ok(Value::Null);
        };
        Ok(self
            .offsets
            .get(index)
            .and_then(|at| content.get(*at))
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
        if name == b"highlight" {
            return self.highlight(context, arguments);
        }
        if name == b"snippet" {
            // **The same snippet, and a different argument order.** FTS3 takes
            // the markers first and the column fourth; FTS5 takes the column
            // first. Rewriting the call here rather than writing the routine
            // twice is what keeps the two dialects from drifting apart.
            if self.dialect == Dialect::Three {
                let start = arguments.first().cloned().unwrap_or(text_value(b"<b>"));
                let end = arguments.get(1).cloned().unwrap_or(text_value(b"</b>"));
                let ellipsis = arguments
                    .get(2)
                    .cloned()
                    .unwrap_or(text_value(b"<b>...</b>"));
                let column = arguments
                    .get(3)
                    .and_then(Value::as_integer)
                    .unwrap_or(-1)
                    .max(0);
                let tokens = arguments.get(4).and_then(Value::as_integer).unwrap_or(15);
                return self.snippet(
                    context,
                    &[
                        Value::Integer(column),
                        start,
                        end,
                        ellipsis,
                        Value::Integer(tokens),
                    ],
                );
            }
            return self.snippet(context, arguments);
        }
        if name == b"offsets" && self.dialect == Dialect::Three {
            return self.offsets(context);
        }
        if name == b"matchinfo" && self.dialect == Dialect::Three {
            return self.matchinfo(context, arguments);
        }
        // **`optimize(t)`, which is FTS3/4's and always has nothing to do
        // here.** SQLite merges an FTS3/4 index's b-tree segments into one and
        // answers `Index already optimal` when there is nothing to merge. This
        // index keeps one doclist per term - see `stage_doclist` - so there are
        // never segments to merge and the answer is the second case, always.
        // Saying so is the truthful answer; refusing the name was not, and it
        // was the last function of SQLite's register that this engine answered
        // to by another route and did not name.
        if name == b"optimize" && self.dialect == Dialect::Three {
            return Value::owned_text(b"Index already optimal");
        }
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
        for (slot, argument) in weights.iter_mut().zip(arguments.iter()) {
            *slot = argument.as_real().unwrap_or(1.0);
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

/// Returns how often one phrase occurs, in this row and in the table.
///
/// Three numbers, which is what `matchinfo`'s `x` reports per phrase and
/// column: the occurrences in this row, the occurrences in every row, and how
/// many rows hold at least one.
///
/// @param hits - where the phrase matched, per row
/// @param rowid - the row being reported on
/// @param column - which column
fn phrase_counts(hits: &expr::Hits, rowid: i64, column: usize) -> (i64, i64, i64) {
    let mut here = 0i64;
    let mut every = 0i64;
    let mut rows = 0i64;
    for (held, positions) in hits.iter() {
        let occurrences = positions
            .iter()
            .filter(|(at, _)| *at == column)
            .map(|(_, offsets)| offsets.len() as i64)
            .sum::<i64>();
        if occurrences == 0 {
            continue;
        }
        every = every.saturating_add(occurrences);
        rows = rows.saturating_add(1);
        if *held == rowid {
            here = occurrences;
        }
    }
    (here, every, rows)
}

/// Returns a text value, for a default an argument did not supply.
///
/// @param bytes - the text
fn text_value(bytes: &[u8]) -> Value<'static> {
    Value::owned_text(bytes).unwrap_or(Value::Null)
}

/// Returns a function argument as bytes, or nothing for a NULL.
///
/// @param value - the argument, when there is one
fn text_argument(value: Option<&Value<'static>>) -> Vec<u8> {
    match value {
        Some(Value::Text(text)) => text.utf8_bytes().into_owned(),
        Some(Value::Blob(blob)) => blob.raw().to_vec(),
        _ => Vec::new(),
    }
}

/// Reports whether a token is one the query asked for.
///
/// A prefix term matches anything starting with it, which is what the trailing
/// `*` means and is why this is not an equality.
///
/// @param token - the folded token from the text
/// @param wanted - the query's terms
fn matches_a_term(token: &[u8], wanted: &[expr::Term]) -> bool {
    wanted.iter().any(|term| {
        if term.prefix {
            token.starts_with(&term.token)
        } else {
            token == term.token.as_slice()
        }
    })
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

    /// The run appender writes the bytes the entry appender would.
    ///
    /// **Two ways to write a doclist entry is a format with two definitions**,
    /// and the bulk-load path takes the one that never builds an entry - so if
    /// the two ever disagree, an index built by a bulk load and one built a row
    /// at a time hold different bytes for the same documents, and only one of
    /// them decodes.
    ///
    /// The cases are the shapes that separate them: one column and one
    /// position, one column and several, two columns, a gap between column
    /// numbers, and a position of zero.
    #[test]
    fn a_run_appends_the_bytes_the_entry_would() {
        let runs: Vec<Vec<(Vec<u8>, usize, u32)>> = vec![
            vec![(b"a".to_vec(), 0, 0)],
            vec![(b"a".to_vec(), 0, 3)],
            vec![
                (b"a".to_vec(), 0, 0),
                (b"a".to_vec(), 0, 3),
                (b"a".to_vec(), 0, 9),
            ],
            vec![
                (b"a".to_vec(), 0, 2),
                (b"a".to_vec(), 1, 0),
                (b"a".to_vec(), 1, 1),
            ],
            vec![(b"a".to_vec(), 0, 1), (b"a".to_vec(), 3, 7)],
            vec![
                (b"a".to_vec(), 1, 0),
                (b"a".to_vec(), 1, 4),
                (b"a".to_vec(), 2, 2),
                (b"a".to_vec(), 2, 3),
            ],
        ];
        for run in &runs {
            for delta in [1i64, 7, 300] {
                let mut from_run = Vec::new();
                append_run(&mut from_run, delta, run);
                let mut from_entry = Vec::new();
                append_doclist_entry(
                    &mut from_entry,
                    delta,
                    &DocEntry {
                        rowid: 0,
                        columns: columns_of(run),
                    },
                );
                assert_eq!(
                    from_run, from_entry,
                    "the two appenders disagree for {run:?} at delta {delta}"
                );
            }
        }
    }

    /// A run groups into the per-column form an entry holds.
    #[test]
    fn a_run_groups_by_column() {
        let run = vec![
            (b"a".to_vec(), 0, 1),
            (b"a".to_vec(), 0, 4),
            (b"a".to_vec(), 2, 0),
        ];
        assert_eq!(columns_of(&run), vec![(0, vec![1, 4]), (2, vec![0])]);
        assert!(columns_of(&[]).is_empty());
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
