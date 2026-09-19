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
//!   token count per column - and nothing else that this build writes. A file
//!   written before task-1911 also has one row per term here; see below.
//! - `%_idx(segid, term, doclist)` is the term dictionary **and** the term's
//!   whole doclist, in one row: `segid` is always zero, `term` is the term's
//!   bytes, and `doclist` is the postings this build used to keep in a
//!   separate `%_data` row. Reading or writing a term is one row.
//!
//! ## One row per term, not two
//!
//! Building the index used to cost two tree writes per term touched - the
//! dictionary row naming a `%_data` page, and that page's doclist - because
//! the dictionary was designed to point at a segment the way SQLite's does,
//! and this index never grew the segments that would have made the
//! indirection earn its keep. Measured on `extension.fts.build`'s 500-document
//! corpus, the dictionary write and the doclist write together were about 2.9
//! of the workload's 10.97 ms. Folding them into the one row a term's key
//! already identifies removes one of the two writes and, for the common path
//! of a term already open this transaction, one of the two reads.
//!
//! **A row is self-describing, not the table.** `%_idx`'s third column holds
//! an `Integer` for a page number when an older build wrote the row and this
//! build has not touched it since, or a `Blob` for the doclist inline when
//! this build wrote it. `term_value` is where that is decided, and it is
//! decided per row rather than by a schema version in `%_config`, because the
//! truth is per row: a file can hold both kinds side by side while it is being
//! written to gradually, and every write from this build replaces whatever it
//! touches with the inline form. A term that is never written again keeps
//! answering through the old indirection for as long as the file exists; the
//! `rebuild` command converts every row at once, on request, because it
//! already reads every row out of `%_content` and writes the index from
//! scratch.
//!
//! A doclist is a run of entries, each: the rowid as a delta from the previous
//! one, then per column that has a position, the column number, how many
//! positions, and the positions as deltas. Everything is a varint, which is
//! what makes a doclist for a common word small enough to be worth keeping in
//! one row.

// —— the module, in the four things it does (task-1962, A8) ————————————
//
// 2,933 lines in one file, with `Fts5Table`'s two `impl` blocks nine hundred
// lines apart and the pending buffer's twenty free functions between them.
// Every item is re-exported, so nothing outside this directory changed.
use self::index::*;
use self::query::*;
mod index;
mod merge;
mod query;

pub use index::{build_stages, decode_sizes, reset_build_stages, Buffer, BuildStages, Pending};

pub mod bm25;
mod doclist;
pub mod expr;
pub mod options;
pub mod tokenize;
pub mod vocab;

use std::sync::Arc;

use inillucent_base::DbResult;
use inillucent_value::Value;

use super::{
    constraint, Change, ConstraintOp, Context, Declaration, DeclaredColumn, IndexQuery, Module,
    ModuleArguments, ShadowTable, VirtualCursor, VirtualTable, ROWID_COLUMN,
};
use crate::shadow::ShadowTables;

use self::doclist::{decode_doclist, doclist_rows, DocEntry};
use self::options::{parse_options, unsupported_option, ColumnSpec, Options};
use self::tokenize::Tokenizer;

/// The `%_data` row that holds the totals.
const TOTALS: i64 = 1;
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
                // The third column is untyped, so it takes an integer page
                // number from a file an older build wrote just as readily as
                // the blob doclist this build writes - see the module's own
                // doc comment for why the two coexist.
                create_sql:
                    "CREATE TABLE \"%_idx\"(segid, term, doclist, PRIMARY KEY(segid, term)) \
                     WITHOUT ROWID"
                        .to_string(),
                owner: None,
            },
            // **Named rather than made** for an external content table: the
            // rows already exist in somebody else's table, and creating a
            // second, empty `%_content` beside them would be an index over
            // nothing. A contentless table has no content shadow at all, which
            // is the whole of what `content=''` asks for, so its entry is
            // dropped below rather than written here.
            match &options.content {
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
        ]
        .into_iter()
        .filter(|shadow| !(options.contentless && shadow.suffix == b"content"))
        .collect())
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
        // A contentless table has no content shadow to ask for, and asking for
        // one it does not have is a refusal: `ShadowTables::of` treats a
        // missing shadow as a corrupt schema, which is right everywhere else.
        let mut wanted: Vec<&[u8]> = vec![b"data", b"idx"];
        if !options.contentless {
            wanted.push(suffix);
        }
        wanted.push(b"docsize");
        wanted.push(b"config");
        // **Refused here only when the table is being created.** A table an
        // older build or SQLite wrote carries the same option, and refusing it
        // on reconnect would stop the database opening at all - so the refusal
        // moves to the queries, where a caller can still drop the table.
        if creating {
            if let Some(what) = &options.unsupported {
                return Err(unsupported_option(what));
            }
        }
        let shadows = ShadowTables::of(arguments, &wanted)?;
        Ok(Box::new(Fts5Table {
            dialect,
            tokenizer: Tokenizer::named(&options.tokenizer)?,
            name: arguments.table.clone(),
            content: suffix.to_vec(),
            external: options.content.clone(),
            contentless: options.contentless,
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
    /// The table's own name, which its refusals name.
    name: Vec<u8>,
    /// The shadow suffix the rows are reached under.
    ///
    /// `content` for a table that owns its rows and the empty name for one
    /// whose rows belong to somebody else, because a granted shadow is looked
    /// up under the owner's own name rather than under a derived one.
    content: Vec<u8>,
    /// The table the rows belong to, when they are not this table's.
    external: Option<Vec<u8>>,
    /// Whether `content=''` made the table contentless.
    ///
    /// There is no `%_content` to read, so every declared column answers NULL
    /// and nothing is written - see [`Options::contentless`] for what was
    /// happening instead.
    contentless: bool,
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

    /// Returns whether the table stores no rows of its own at all.
    ///
    /// True for a contentless table and for an external content one: both are
    /// the cases where this module writes no document text, and both are the
    /// cases `delete-all` is for.
    fn stores_no_rows(&self) -> bool {
        self.contentless || self.external.is_some()
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
            unsupported: self.options.unsupported.clone(),
            content: self.content.clone(),
            contentless: self.contentless,
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
                        // `%_docsize` stands in for a contentless table, which
                        // has no `%_content` to read the highest rowid out of.
                        let suffix = match self.contentless {
                            true => b"docsize".to_vec(),
                            false => self.content.clone(),
                        };
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
        // The check reads `%_idx` rows rather than staged doclists, so it is
        // the one reader the buffer cannot answer: written out first.
        flush_doclists(context, &self.shadows, &self.pending)?;
        let mut problems = Vec::new();
        let mut rows = Vec::new();
        let suffix = match self.stores_no_rows() {
            true => b"docsize".to_vec(),
            false => self.content.clone(),
        };
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
        let mut terms: Vec<(Vec<u8>, Vec<Value<'static>>)> = Vec::new();
        self.shadows.scan_keyed(context, b"idx", 2, |values| {
            if let Some(term) = values.get(1).and_then(Value::as_blob) {
                terms.push((term.raw().to_vec(), values.to_vec()));
            }
            Ok(true)
        })?;
        for (term, row) in &terms {
            let Some(bytes) = resolve_doclist(context, &self.shadows, row)? else {
                problems.push(format!(
                    "the term {} has no doclist",
                    String::from_utf8_lossy(term)
                ));
                continue;
            };
            for entry in decode_doclist(&bytes) {
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
    use super::doclist::append_doclist_entry;
    use super::*;

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
}
