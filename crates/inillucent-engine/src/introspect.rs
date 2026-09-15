//! The tables that describe *statements* rather than data.
//!
//! Invariant: **each of these answers about this connection, from this
//! connection**, and none of them is a second implementation of something the
//! engine already knows. `bytecode` reads the same listing a plain `EXPLAIN`
//! prints, `tables_used` reads the same plan `EXPLAIN QUERY PLAN` describes,
//! `sqlite_stmt` reads the statement cache itself, and `completion` reads the
//! keyword table and the catalog. A second answer that could drift from the
//! first would be worse than no answer.
//!
//! They are the engine's rather than a module's for the same reason the
//! `pragma_*` functions are: a [`crate::vtab`] module reaches its own shadow
//! tables and nothing else, and every question here is about the connection.
//!
//! What they do *not* promise is SQLite's numbers. `bytecode` lists this
//! engine's operator chain because this engine compiles no bytecode, and
//! `sqlite_stmt`'s counters are the ones this engine keeps. The shape - the
//! column names, their order, which are hidden - is SQLite's exactly, so a
//! query written against one runs against the other.

use inillucent_base::DbResult;
use inillucent_tree::datum::OwnedDatum;

use crate::ImportedDatabase;

/// The columns `bytecode(SQL)` answers with, the hidden one last.
pub const BYTECODE_COLUMNS: &[&str] = &[
    "addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment", "subprog", "nexec", "ncycle",
];

/// The columns `tables_used(SQL)` answers with, the hidden one last.
pub const TABLES_USED_COLUMNS: &[&str] = &["type", "schema", "name", "wr", "subprog"];

/// The columns `sqlite_stmt` answers with. None of them is hidden.
pub const STMT_COLUMNS: &[&str] = &[
    "sql", "ncol", "ro", "busy", "nscan", "nsort", "naidx", "nstep", "reprep", "run", "mem",
];

/// The columns `completion(PREFIX, WHOLELINE)` answers with.
pub const COMPLETION_COLUMNS: &[&str] = &["candidate"];

/// The hidden columns `completion` carries, in order.
pub const COMPLETION_HIDDEN: &[&str] = &["prefix", "wholeline", "phase"];

/// Which part of the schema a completion candidate came from.
///
/// SQLite's own numbering, gaps included: the phases between a keyword and a
/// database name are `PRAGMAS`, `FUNCTIONS`, `COLLATIONS`, `INDEXES` and
/// `TRIGGERS`, and its own implementation walks past all five without
/// producing a row. Keeping the numbers means a caller that filters on
/// `phase` filters the same way here.
const PHASE_KEYWORD: i64 = 1;
/// A database's name.
const PHASE_DATABASE: i64 = 7;
/// A table's, view's or trigger's name.
const PHASE_TABLE: i64 = 8;
/// A column's name.
const PHASE_COLUMN: i64 = 9;

impl ImportedDatabase {
    /// Returns the listing `bytecode(SQL)` answers with.
    ///
    /// The same rows a plain `EXPLAIN` of the statement prints, with the three
    /// columns SQLite adds for a run that has happened: `subprog` names the
    /// sub-program a step belongs to, and `nexec` and `ncycle` count what it
    /// did. This engine measures neither per step, so they are zero - which is
    /// what SQLite answers too until the statement has been run under
    /// `SQLITE_ENABLE_STMT_SCANSTATUS`.
    ///
    /// @param sql - the statement to list
    pub(crate) fn bytecode_rows(&self, sql: &[u8]) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let text = String::from_utf8_lossy(sql).into_owned();
        let listing = self.program_listing(&text)?;
        Ok(listing
            .into_iter()
            .enumerate()
            .map(|(address, (opcode, one, two, argument, comment))| {
                vec![
                    OwnedDatum::Int(address as i64),
                    OwnedDatum::Text(opcode.into_bytes()),
                    OwnedDatum::Int(one),
                    OwnedDatum::Int(two),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(argument.into_bytes()),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(comment.into_bytes()),
                    OwnedDatum::Null,
                    OwnedDatum::Int(0),
                    OwnedDatum::Int(0),
                ]
            })
            .collect())
    }

    /// Returns the rows `tables_used(SQL)` answers with.
    ///
    /// One row per object the statement reads or writes, with `wr` set for the
    /// one it writes. **The plan's sources rather than the parse's**, so a view
    /// is reported as the view a caller named and a subquery contributes the
    /// tables it reads - which is the question somebody auditing a statement is
    /// asking.
    ///
    /// @param sql - the statement to describe
    pub(crate) fn tables_used_rows(&self, sql: &[u8]) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let text = String::from_utf8_lossy(sql).into_owned();
        let (names, written) = self.statement_tables(&text)?;
        Ok(names
            .into_iter()
            .map(|(kind, name)| {
                let writes = i64::from(written.as_deref() == Some(name.as_slice()));
                vec![
                    OwnedDatum::Text(kind.as_bytes().to_vec()),
                    OwnedDatum::Text(b"main".to_vec()),
                    OwnedDatum::Text(name),
                    OwnedDatum::Int(writes),
                    OwnedDatum::Null,
                ]
            })
            .collect())
    }

    /// Returns the rows `sqlite_stmt` answers with.
    ///
    /// One row per statement this connection has compiled and kept. The
    /// counters are the ones this engine has: `ncol` is how many columns the
    /// statement answers with, `ro` whether it only reads, and the rest are
    /// zero because they count VDBE work this engine does not do. `busy` is
    /// zero for every row, because a cached statement is not mid-execution -
    /// the one that is, is the statement doing the asking, and it is not in the
    /// cache until it finishes compiling.
    pub(crate) fn stmt_rows(&self) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let mut texts: Vec<String> = Vec::new();
        for session in self.compiled.statements.borrow().values() {
            texts.extend(session.keys().cloned());
        }
        texts.sort();
        let mut rows = Vec::with_capacity(texts.len());
        for text in texts {
            let (columns, reads) = self.statement_shape(&text);
            rows.push(vec![
                OwnedDatum::Text(text.into_bytes()),
                OwnedDatum::Int(columns),
                OwnedDatum::Int(i64::from(reads)),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
            ]);
        }
        Ok(rows)
    }

    /// Returns the rows `completion(PREFIX)` answers with.
    ///
    /// Every keyword, then every attached database's name, then every object in
    /// every schema, then every column of every table - which is SQLite's own
    /// four phases and its own order. A prefix filters them case-insensitively;
    /// an empty prefix keeps them all.
    ///
    /// @param prefix - what has been typed so far, when anything has
    pub(crate) fn completion_rows(&self, prefix: &[u8]) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let mut candidates: Vec<(Vec<u8>, i64)> = Vec::new();
        for keyword in inillucent_sql::keyword::KEYWORDS {
            candidates.push((keyword.text().to_vec(), PHASE_KEYWORD));
        }
        // **The schema as `sqlite_schema` shows it**, not as the binder's
        // catalog holds it. The catalog carries the eponymous virtual tables
        // and the temporary database whether or not anything is in them, and a
        // completion list that offered `pragma_table_info` as a table name
        // would be offering something a person never types.
        let attached = self.pragma_rows(b"database_list", None)?;
        for row in attached.map(|answer| answer.rows).unwrap_or_default() {
            if let Some(name) = text_of(row.get(1)) {
                candidates.push((name, PHASE_DATABASE));
            }
        }
        let named: Vec<Vec<u8>> = self
            .main_entries()
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        for name in &named {
            candidates.push((name.clone(), PHASE_TABLE));
        }
        for name in &named {
            let Some(table) = self.schema.catalog.table_named(&name.to_ascii_lowercase()) else {
                continue;
            };
            for column in &table.columns {
                candidates.push((column.name.clone(), PHASE_COLUMN));
            }
        }
        let folded = prefix.to_ascii_lowercase();
        Ok(candidates
            .into_iter()
            .filter(|(candidate, _)| {
                folded.is_empty() || candidate.to_ascii_lowercase().starts_with(&folded)
            })
            .map(|(candidate, phase)| {
                vec![
                    OwnedDatum::Text(candidate),
                    OwnedDatum::Text(prefix.to_vec()),
                    OwnedDatum::Null,
                    OwnedDatum::Int(phase),
                ]
            })
            .collect())
    }
}

/// Returns a datum's bytes when it holds text.
///
/// @param value - the column, when the row had one
fn text_of(value: Option<&OwnedDatum>) -> Option<Vec<u8>> {
    match value {
        Some(OwnedDatum::Text(bytes)) => Some(bytes.clone()),
        _ => None,
    }
}
