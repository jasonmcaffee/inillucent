//! `fts5vocab`: the terms an FTS5 index holds, as a table.
//!
//! Invariant: it is a **read-only view over another table's index**, and it
//! never touches anything but that index. A vocabulary table is how a person
//! answers "what words are in this corpus and how common are they" - the
//! question a stop-word list, a spelling suggestion and a relevance experiment
//! all start from - and the only way to answer it is to read the term
//! dictionary the index already built.
//!
//! That is why `ShadowTable::owner` exists. The module contract says a module
//! sees only what it was handed, which is what makes a hostile module a bounded
//! problem; this module is handed another table's shadows *by name*, they are
//! looked up rather than created, and it still cannot resolve a name for
//! itself. The reach is one explicit grant rather than a hole in the contract.
//!
//! # The three shapes
//!
//! | `USING fts5vocab(f, ...)` | columns | one row per |
//! |---|---|---|
//! | `'row'` | `term, doc, cnt` | term |
//! | `'col'` | `term, col, doc, cnt` | term and column |
//! | `'instance'` | `term, doc, col, offset` | occurrence |
//!
//! `doc` counts documents in the first two and *is* a document's rowid in the
//! third, which is SQLite's naming and is worth reading twice.

use inillucent_base::{varint, DbResult};
use inillucent_value::Value;

use super::super::{
    failure, Context, Declaration, DeclaredColumn, FilterPlan, IndexQuery, Module, ModuleArguments,
    ShadowTable, VirtualCursor, VirtualTable,
};
use crate::shadow::ShadowTables;

/// Which of the three shapes a vocabulary table has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// One row per term.
    Row,
    /// One row per term and column.
    Column,
    /// One row per occurrence.
    Instance,
}

impl Shape {
    /// Returns the shape a `'row'`, `'col'` or `'instance'` argument names.
    ///
    /// @param written - the second argument, as written
    fn named(written: &[u8]) -> Option<Shape> {
        match written.to_ascii_lowercase().as_slice() {
            b"row" => Some(Shape::Row),
            b"col" => Some(Shape::Column),
            b"instance" => Some(Shape::Instance),
            _ => None,
        }
    }

    /// Returns the columns the shape declares, in order.
    fn columns(self) -> &'static [&'static str] {
        match self {
            Shape::Row => &["term", "doc", "cnt"],
            Shape::Column => &["term", "col", "doc", "cnt"],
            Shape::Instance => &["term", "doc", "col", "offset"],
        }
    }
}

/// The `fts5vocab` module.
pub struct Fts5VocabModule;

/// Returns the target table and shape a `fts5vocab(...)` names.
///
/// The first argument is the FTS5 table and the second is the shape. SQLite
/// also accepts a schema name before the table; a two-argument call is the form
/// everything writes and the only one accepted here, so a three-argument call
/// is refused rather than half-read.
///
/// @param arguments - the arguments inside the parentheses
fn parse_arguments(arguments: &[Vec<u8>]) -> DbResult<(Vec<u8>, Shape)> {
    let (Some(table), Some(shape)) = (arguments.first(), arguments.get(1)) else {
        return Err(failure(
            "fts5vocab: expected a table name and one of 'row', 'col' or 'instance'",
        ));
    };
    let unquoted = unquote(shape);
    let Some(shape) = Shape::named(&unquoted) else {
        return Err(failure(format!(
            "fts5vocab: unknown table type: {}",
            String::from_utf8_lossy(&unquoted)
        )));
    };
    Ok((unquote(table), shape))
}

/// Returns an argument with any surrounding quotes removed.
///
/// `fts5vocab(f, 'row')` writes the shape as a string literal and the table as
/// a bare word, and both arrive here as the source text they were written as.
///
/// @param written - the argument as written
fn unquote(written: &[u8]) -> Vec<u8> {
    let trimmed: &[u8] = match written.split_first() {
        Some((b'\'', rest)) | Some((b'"', rest)) => {
            rest.split_last().map(|(_, held)| held).unwrap_or(rest)
        }
        _ => written,
    };
    trimmed.to_vec()
}

impl Module for Fts5VocabModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        "fts5vocab"
    }

    /// The index's own shadows, read rather than made.
    ///
    /// Only the two the vocabulary needs: `%_idx` is the term dictionary and
    /// `%_data` holds each term's doclist. The content and the sizes are the
    /// index's own business and are not asked for.
    fn shadow_tables(&self, arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        let (target, _) = parse_arguments(&arguments.arguments)?;
        Ok(vec![
            ShadowTable {
                suffix: b"idx".to_vec(),
                create_sql: String::new(),
                owner: Some(target.clone()),
            },
            ShadowTable {
                suffix: b"data".to_vec(),
                create_sql: String::new(),
                owner: Some(target),
            },
        ])
    }

    /// Connects, which is deciding the shape and declaring its columns.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        _creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        let (_, shape) = parse_arguments(&arguments.arguments)?;
        let (target, _) = parse_arguments(&arguments.arguments)?;
        Ok(Box::new(VocabTable {
            shape,
            target,
            shadows: ShadowTables::of(arguments, &[b"idx", b"data"])?,
            declaration: Declaration {
                columns: shape
                    .columns()
                    .iter()
                    .map(|name| DeclaredColumn::visible(name))
                    .collect(),
                without_rowid: false,
            },
        }))
    }
}

/// Returns the target index's column names, for the `col` column to report.
///
/// **Not read from the index**, because the index does not store them: it
/// stores column *numbers*, and the names are in the target's declaration. The
/// declaration is in the catalog, which a module is shown through
/// `Context::catalog` - the same route `pragma_table_info` takes, and the
/// reason that field is on the context at all.
///
/// A name the catalog does not have is reported as `cN`, which is what the
/// index itself calls it. That is the honest answer for a target that has gone
/// rather than a guess at what it used to be called.
///
/// @param context - the statement's context
/// @param target - the FTS5 table the vocabulary is over
fn column_names(context: &Context<'_>, target: &[u8]) -> Vec<Vec<u8>> {
    let folded = target.to_ascii_lowercase();
    let Some(catalog) = context.catalog else {
        return Vec::new();
    };
    catalog
        .table_named(&folded)
        .map(|table| {
            table
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// One connected vocabulary table.
struct VocabTable {
    shape: Shape,
    /// The FTS5 table this is a vocabulary of, for its column names.
    target: Vec<u8>,
    shadows: ShadowTables,
    declaration: Declaration,
}

impl VirtualTable for VocabTable {
    /// Returns the shape's columns.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Takes nothing: the whole dictionary is read and the engine filters.
    ///
    /// A term lookup could be a seek into `%_idx`, and it is not one yet -
    /// which costs a scan of the dictionary on `WHERE term = 'x'`. The cost is
    /// bounded by the vocabulary rather than by the corpus, and reporting a
    /// constraint the module then had to honour exactly is how a module returns
    /// wrong rows; taking none is the safe half of the contract.
    fn best_index(&self, info: &mut IndexQuery) -> DbResult<()> {
        info.index_number = 0;
        info.estimated_cost = 1000.0;
        Ok(())
    }

    /// Opens a cursor, which reads the whole dictionary at once.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(VocabCursor {
            shape: self.shape,
            target: self.target.clone(),
            columns: Vec::new(),
            shadows: self.shadows.clone(),
            rows: Vec::new(),
            at: 0,
        }))
    }
}

/// One row of a vocabulary table, in the shape's own column order.
type VocabRow = Vec<Value<'static>>;

/// A cursor over one vocabulary table.
struct VocabCursor {
    shape: Shape,
    /// The FTS5 table this is a vocabulary of.
    target: Vec<u8>,
    /// Its column names, resolved when the scan runs.
    columns: Vec<Vec<u8>>,
    shadows: ShadowTables,
    rows: Vec<VocabRow>,
    at: usize,
}

impl VirtualCursor for VocabCursor {
    /// Reads the dictionary and expands every term's doclist.
    ///
    /// The whole thing at once, in term order, because the dictionary is keyed
    /// by term and a scan of it is already sorted - which is the order every
    /// caller of a vocabulary table wants.
    fn filter(&mut self, context: &mut Context<'_>, _plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.at = 0;
        self.columns = column_names(context, &self.target);
        // `%_idx(segid, term, doclist)`, keyed on the first two - or, for a
        // row an older build wrote and nothing has rewritten since, `doclist`
        // is still the page number that build left it under. `resolve_doclist`
        // tells the two apart.
        let mut dictionary: Vec<(Vec<u8>, Vec<Value<'static>>)> = Vec::new();
        self.shadows.scan_keyed(context, b"idx", 2, |values| {
            let term = match values.get(1) {
                Some(Value::Text(text)) => text.utf8_bytes().into_owned(),
                Some(Value::Blob(blob)) => blob.raw().to_vec(),
                _ => return Ok(true),
            };
            dictionary.push((term, values.to_vec()));
            Ok(true)
        })?;
        for (term, row) in dictionary {
            let Some(doclist) = super::resolve_doclist(context, &self.shadows, &row)? else {
                continue;
            };
            self.expand(&term, &doclist);
        }
        Ok(())
    }

    /// Moves to the next row.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    /// Returns whether the cursor is past the last row.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column of the current row.
    fn column(&mut self, _context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        Ok(self
            .rows
            .get(self.at)
            .and_then(|row| row.get(index))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Returns the row's position, which is the only rowid it has.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.at.saturating_add(1) as i64)
    }
}

impl VocabCursor {
    /// Turns one term's doclist into the rows the shape asks for.
    ///
    /// The doclist is a run of entries: the rowid as a delta from the previous,
    /// then per column that has a position, the column number, how many
    /// positions, and the positions as deltas. Everything a vocabulary table
    /// reports is a count over that, which is why all three shapes are one
    /// walk with three different accumulators.
    ///
    /// @param term - the term the doclist belongs to
    /// @param bytes - the doclist
    fn expand(&mut self, term: &[u8], bytes: &[u8]) {
        let mut at = 0usize;
        let mut rowid = 0i64;
        // For `row` and `col`: how many documents and how many occurrences.
        let mut documents = 0i64;
        let mut occurrences = 0i64;
        let mut per_column: Vec<(usize, i64, i64)> = Vec::new();
        while at < bytes.len() {
            rowid = rowid.wrapping_add(read_varint(bytes, &mut at) as i64);
            let groups = read_varint(bytes, &mut at) as usize;
            if groups > 4096 {
                break;
            }
            let mut counted_document = false;
            for _ in 0..groups {
                let column = read_varint(bytes, &mut at) as usize;
                let positions = read_varint(bytes, &mut at) as usize;
                if positions > 1 << 24 {
                    return;
                }
                let mut offset = 0i64;
                for _ in 0..positions {
                    offset = offset.wrapping_add(read_varint(bytes, &mut at) as i64);
                    if self.shape == Shape::Instance {
                        self.rows.push(vec![
                            text(term),
                            Value::Integer(rowid),
                            self.column_name(column),
                            Value::Integer(offset),
                        ]);
                    }
                }
                if positions == 0 {
                    continue;
                }
                if !counted_document {
                    documents = documents.saturating_add(1);
                    counted_document = true;
                }
                occurrences = occurrences.saturating_add(positions as i64);
                match per_column.iter_mut().find(|(held, _, _)| *held == column) {
                    Some((_, docs, count)) => {
                        *docs = docs.saturating_add(1);
                        *count = count.saturating_add(positions as i64);
                    }
                    None => per_column.push((column, 1, positions as i64)),
                }
            }
        }
        match self.shape {
            Shape::Instance => {}
            Shape::Row => self.rows.push(vec![
                text(term),
                Value::Integer(documents),
                Value::Integer(occurrences),
            ]),
            Shape::Column => {
                per_column.sort_unstable_by_key(|(column, _, _)| *column);
                for (column, docs, count) in per_column {
                    self.rows.push(vec![
                        text(term),
                        self.column_name(column),
                        Value::Integer(docs),
                        Value::Integer(count),
                    ]);
                }
            }
        }
    }

    /// Returns a column's name, or its number when nobody supplied one.
    ///
    /// @param column - the column's position in the index
    fn column_name(&self, column: usize) -> Value<'static> {
        match self.columns.get(column) {
            Some(name) => text(name),
            None => text(format!("c{column}").as_bytes()),
        }
    }
}

/// Returns a text value over some bytes.
fn text(bytes: &[u8]) -> Value<'static> {
    Value::owned_text(bytes).unwrap_or(Value::Null)
}

/// Reads one varint, advancing the cursor.
///
/// The same decoding the index's own reader does; a malformed run stops the
/// walk rather than looping, which is what the cursor jump to the end is for.
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
