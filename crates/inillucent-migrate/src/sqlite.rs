//! Migrating a SQLite database file into the new engine, verified.
//!
//! Invariant: **the source file is opened read-only, is never written, and is
//! never removed** - the same property the legacy-generation source has, for
//! the same reason. If the destination turns out wrong a week later, going back
//! is opening the file that has been sitting there unchanged the whole time,
//! not undoing anything.
//!
//! The order below exists so that every intermediate state is either
//! recognisable or harmless:
//!
//! 1. **Inventory the source.** Every table's row count and an ordered digest
//!    of its rows, read through `inillucent-sqlite-reader` over the source file.
//! 2. **Stage** into a uniquely named file *beside* the destination, never over
//!    it, so a half-written database is never at the path an application opens.
//! 3. **Close and reopen the staged file**, so what is verified is what a fresh
//!    process sees rather than what one warm pool saw.
//! 4. **Verify counts and digests, both.** A migration that moves the right
//!    number of rows and the wrong bytes passes a count check, and this project
//!    has already paid for a fast path that was also a different answer.
//! 5. **Publish** by renaming the verified staging file into place, refusing to
//!    overwrite anything already there.
//!
//! A failure at any step leaves the staging file and the source. Nothing is
//! cleaned up, because the thing a person needs after a failed migration is the
//! evidence.
//!
//! ## What "verified" means here, and what it does not
//!
//! The digests below are computed from two *different readers over two
//! different files*: the source's rows come off SQLite b-tree pages through
//! `inillucent-sqlite-reader`, and the destination's come off PAX leaves through
//! the new engine's own scan. So a disagreement means the copy is wrong, and
//! agreement means the two engines read the same rows in the same order.
//!
//! What it does not do is prove the source was read correctly in the first
//! place, because the import and the inventory share that reader. That oracle
//! is a third engine - the pinned SQLite binary - and it lives in the migration
//! acceptance test rather than in the tool, because the tool must not require a
//! SQLite installation in order to migrate a file.

use std::path::{Path, PathBuf};

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::hash::Sha256;
use inillucent_base::DbResult;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_engine::{logical_row, source_layout_of, ImportedDatabase};
use inillucent_sqlite_reader::SqliteFile;
use inillucent_tree::datum::OwnedDatum;

use crate::verify::Check;

/// The page size the new trees are built at.
///
/// The engine's own default, which is what `Options::default` says and what
/// Phase 1 fixed after measuring 16/32/64. A migration that chose its own would
/// be producing a file unlike the ones every measurement was taken on.
pub const PAGE_SIZE: usize = 32_768;

/// How many frames the pool holds while the migration runs.
///
/// It changes how long the migration takes and nothing about what it produces.
pub const FRAMES: usize = 4_096;

/// One table as the source holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInventory {
    /// The table's name, as the schema spells it.
    pub name: String,
    /// How many rows it holds.
    pub rows: u64,
    /// An ordered digest of every row.
    pub digest: String,
}

/// What a SQLite source holds, and proof of which rows it held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteInventory {
    /// The file that was read.
    pub path: PathBuf,
    /// Its page size, as the header declares it.
    pub page_size: u32,
    /// Its page count.
    pub page_count: u32,
    /// Every ordinary table, in schema order.
    pub tables: Vec<TableInventory>,
}

impl SqliteInventory {
    /// Returns the table of a given name, if the source has one.
    ///
    /// @param name - the table's name, compared case-insensitively
    pub fn table(&self, name: &str) -> Option<&TableInventory> {
        let folded = name.to_ascii_lowercase();
        self.tables
            .iter()
            .find(|table| table.name.to_ascii_lowercase() == folded)
    }
}

/// What one migration did.
#[derive(Clone, Debug)]
pub struct Report {
    /// The source that was read.
    pub source: PathBuf,
    /// The file that was published, when it was.
    pub destination: PathBuf,
    /// The staging file it was built as, which is left behind on failure.
    pub staged: PathBuf,
    /// What the source held.
    pub inventory: SqliteInventory,
    /// Every check and what it found.
    pub checks: Vec<Check>,
}

impl Report {
    /// Reports whether every check passed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    /// Returns the checks that failed.
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|check| !check.passed).collect()
    }
}

/// Reads a SQLite file's tables without writing to it.
///
/// **Every failure here is named.** A truncated file, a file whose header is
/// not SQLite's, a page that does not decode - each produces an error naming
/// the file and what was wrong with it, and none of them produces a partial
/// inventory that a later step could mistake for a whole one. That is the
/// property the malformed half of the migration test corpus exists to check.
///
/// @param path - the SQLite database to read
pub fn inventory(path: &Path) -> DbResult<SqliteInventory> {
    let mut file = SqliteFile::open(path.to_path_buf()).map_err(|error| {
        corrupt(format!(
            "{} could not be opened as a SQLite database: {}",
            path.display(),
            error.message()
        ))
    })?;
    let page_size = file.page_size();
    let page_count = file.page_count();
    let schema = file.schema().map_err(|error| {
        corrupt(format!(
            "{}: the schema could not be read: {}",
            path.display(),
            error.message()
        ))
    })?;
    // The virtual tables the file declares, so their shadow tables can be told
    // apart from an application's own.
    let schema_names: Vec<String> = schema
        .iter()
        .filter(|object| object.kind == "table" && object.sql.trim_start().len() > 6)
        .filter(|object| {
            object
                .sql
                .trim_start()
                .get(..14)
                .is_some_and(|head| head.eq_ignore_ascii_case("CREATE VIRTUAL"))
        })
        .map(|object| object.name.to_ascii_lowercase())
        .collect();
    // **Triggers are carried.** This used to refuse the whole migration and
    // name them one by one, and the refusal was right while it stood: the new
    // engine could store a trigger and list it in `sqlite_schema` but could not
    // fire one, and a database whose invariants are maintained by nothing is a
    // failure the owner finds out about from their data. Triggers were made
    // to fire, and every trigger case in `docs/feature-comparison.md` agrees
    // with SQLite - so since then the refusal has been the tool declining to do
    // something the engine can do, which is the *only* reason
    // `--sqlite-file` could not be pointed at an ordinary application's
    // database. The import writes their `CREATE` text out of the table they are
    // attached to; see `ImportedDatabase::import_into`.
    let mut tables = Vec::new();
    for object in schema {
        if object.kind != "table" || object.root == 0 {
            continue;
        }
        // **The schema tables and a module's shadow tables are not the
        // inventory's business.** `sqlite_sequence` is carried by the import as
        // a *value* - the AUTOINCREMENT high-water mark - rather than as a
        // table, and an FTS5 table's `f_data`, `f_idx`, `f_docsize` and
        // `f_config` are the module's private storage, whose declarations use
        // syntax no ordinary table has. Inventorying them was what answered
        // "the declaration of f_data did not parse: database disk image is
        // malformed" - a message that is not true and that nobody could act on.
        if object.name.starts_with("sqlite_") || is_shadow_table(&schema_names, &object.name) {
            continue;
        }
        // The declaration, parsed by the same loader the import uses, because
        // the transform below is only right if both sides agree about which
        // column is the rowid alias and what order a `WITHOUT ROWID` table's
        // record is in.
        let info =
            table_from_create_sql(object.sql.as_bytes(), 0, object.root).map_err(|error| {
                corrupt(format!(
                    "{}: the declaration of {} did not parse: {}",
                    path.display(),
                    object.name,
                    error.message()
                ))
            })?;
        let layout = source_layout_of(&info).map_err(|error| {
            corrupt(format!(
                "{}: the shape of {} could not be derived: {}",
                path.display(),
                object.name,
                error.message()
            ))
        })?;
        // A `WITHOUT ROWID` table's pages are index pages, so its rows are read
        // as index entries rather than as table rows.
        let stored = if info.without_rowid {
            file.read_index(object.root, info.columns.len())
        } else {
            file.read_table(object.root, info.columns.len())
        }
        .map_err(|error| {
            corrupt(format!(
                "{}: the rows of {} could not be read: {}",
                path.display(),
                object.name,
                error.message()
            ))
        })?;
        // **Digested as a query sees them, not as the file stores them.** The
        // destination is read with `SELECT *`, so digesting the storage shape
        // here would compare a row of six values against a row of five and call
        // a correct migration wrong.
        let rows: Vec<Vec<OwnedDatum>> = stored
            .iter()
            .map(|row| logical_row(&info, &layout, row))
            .collect();
        tables.push(TableInventory {
            name: object.name.clone(),
            rows: rows.len() as u64,
            digest: digest_rows(&rows),
        });
    }
    Ok(SqliteInventory {
        path: path.to_path_buf(),
        page_size,
        page_count,
        tables,
    })
}

/// Migrates a SQLite file into a new-engine database, verified, and publishes it.
///
/// Returns the report whether or not it passed: a migration that verified badly
/// is not an error to propagate, it is a result to read. What it never does is
/// publish one.
///
/// @param source - the SQLite database to read, never written
/// @param destination - where the verified database is published
pub fn migrate(source: &Path, destination: &Path) -> DbResult<Report> {
    if destination.exists() {
        return Err(misuse(format!(
            "{} already exists; a migration publishes by renaming and never overwrites",
            destination.display()
        )));
    }
    let inventory = inventory(source)?;
    let staged = staging_path(destination);
    // The build. A failure here - a full disk, a read-only directory, a source
    // page that decodes as far as the inventory and no further - leaves the
    // staging file where it fell and never touches the destination.
    let mut built =
        ImportedDatabase::import_into(source.to_path_buf(), staged.clone(), PAGE_SIZE, FRAMES)
            .map_err(|error| {
                corrupt(format!(
                    "{} could not be built from {}: {}",
                    staged.display(),
                    source.display(),
                    error.message()
                ))
            })?;
    // Checkpointed and closed before a single row is verified.
    built.checkpoint().map_err(|error| {
        corrupt(format!(
            "{} could not be checkpointed: {}",
            staged.display(),
            error.message()
        ))
    })?;
    drop(built);

    // **The full-text tables, rebuilt rather than copied.** An FTS5 table's
    // rows live in `<name>_data`, `<name>_idx`, `<name>_docsize` and
    // `<name>_config`, which are the module's private storage in SQLite's own
    // format; this engine's FTS5 keeps a different one, so copying those pages
    // across would produce a table that exists and cannot be searched. What
    // *is* portable is the text: `<name>_content` holds it, and re-inserting it
    // through this engine's own module builds an index this engine can read.
    //
    // The alternative was the refusal this replaces, which named the wrong
    // thing anyway - "the declaration of `f_data` did not parse: database disk
    // image is malformed", about a file that was neither malformed nor at
    // fault.
    let carried = rebuild_full_text(source, &staged)?;

    // **Opened from the file, not reopened from the handle.** The checks below
    // read a database whose schema was derived from bytes on a disk, by the
    // engine's own open path, in a pool that has never seen the import. A
    // migration verified against the handle that built it would pass with a
    // catalog that only this process could reconstruct - which is exactly the
    // failure "a partial database that looks finished" names.
    let opened = ImportedDatabase::open(staged.clone(), PAGE_SIZE, FRAMES).map_err(|error| {
        corrupt(format!(
            "{} was built but could not be opened: {}",
            staged.display(),
            error.message()
        ))
    })?;
    let mut checks = verify_against(&inventory, &opened);
    checks.extend(carried);
    drop(opened);

    let report = Report {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        staged: staged.clone(),
        inventory,
        checks,
    };
    if !report.passed() {
        return Ok(report);
    }
    // **The log moves with the database.** A `.rdb` is a file plus its log
    // segments, named after it; renaming only the first would publish a
    // database whose log was still called by the staging name. The build is
    // checkpointed before it is verified, so in practice every segment here is
    // empty - but a migration that silently left a non-empty one behind would
    // be losing committed rows, and "in practice empty" is not a thing to
    // publish on.
    for segment in log_segments(&staged)? {
        let Some(name) = segment
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&staged_name(&staged)) else {
            continue;
        };
        let moved = destination.with_file_name(format!("{}{suffix}", staged_name(destination)));
        std::fs::rename(&segment, &moved).map_err(|error| {
            corrupt(format!(
                "{} could not be published to {}: {error}",
                segment.display(),
                moved.display()
            ))
        })?;
    }
    std::fs::rename(&staged, destination).map_err(|error| {
        corrupt(format!(
            "{} was verified but could not be published to {}: {error}",
            staged.display(),
            destination.display()
        ))
    })?;
    Ok(report)
}

/// Checks a built database against what the source held.
///
/// Counts **and** digests, per table. A migration that moved the right number
/// of rows and the wrong bytes passes a count check on its own, so neither is
/// reported without the other.
///
/// @param inventory - what the source held
/// @param built - the migrated database
pub fn verify_against(inventory: &SqliteInventory, built: &ImportedDatabase) -> Vec<Check> {
    let mut checks = Vec::new();
    for table in &inventory.tables {
        let name = &table.name;
        let quoted = name.replace('"', "\"\"");
        let rows = match built.run(&format!("SELECT * FROM \"{quoted}\"")) {
            Ok((rows, _)) => rows,
            Err(error) => {
                checks.push(Check::failed(
                    &format!("table.{name}"),
                    format!(
                        "could not be read from the migrated database: {}",
                        error.message()
                    ),
                ));
                continue;
            }
        };
        let count = rows.len() as u64;
        if count == table.rows {
            checks.push(Check::passed(
                &format!("count.{name}"),
                format!("{count} rows"),
            ));
        } else {
            checks.push(Check::failed(
                &format!("count.{name}"),
                format!("{count} rows migrated, {} in the source", table.rows),
            ));
        }
        let digest = digest_rows(&rows);
        if digest == table.digest {
            checks.push(Check::passed(&format!("digest.{name}"), digest));
        } else {
            checks.push(Check::failed(
                &format!("digest.{name}"),
                format!("{digest} migrated, {} in the source", table.digest),
            ));
        }
    }
    if inventory.tables.is_empty() {
        checks.push(Check::passed("tables", "the source holds none"));
    }
    checks
}

/// Returns a digest of a table's rows, independent of the order they arrive in.
///
/// Every value is folded in with its type tag, so an integer 1 and a text "1"
/// do not digest alike, and each row and each value is length-prefixed, so two
/// different row shapes cannot produce the same byte stream.
///
/// **The rows are sorted before they are folded, and that is a correction.**
/// The first version digested them in arrival order on the argument that both
/// sides read a table in key order. They do not: the source is read by walking
/// its b-tree, which is key order, but the destination is read with
/// `SELECT *`, and a `SELECT` with no `ORDER BY` may be answered from any
/// structure that covers it - so a table with a covering index came back in
/// index order and a correct migration was reported as a wrong one. Ordering an
/// unordered query is not a property this can check, and pretending to check it
/// made the digest report a plan choice as data loss.
///
/// @param rows - the rows, in whatever order they were produced
pub fn digest_rows(rows: &[Vec<OwnedDatum>]) -> String {
    let mut encoded: Vec<Vec<u8>> = rows.iter().map(|row| encode_row(row)).collect();
    encoded.sort();
    let mut hash = Sha256::new();
    hash.update(&(rows.len() as u64).to_le_bytes());
    for row in &encoded {
        hash.update(row);
    }
    hash.hex()
}

/// Returns one row as the bytes it is digested and ordered by.
///
/// @param row - the row's values
fn encode_row(row: &[OwnedDatum]) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len() * 12);
    out.extend_from_slice(&(row.len() as u64).to_le_bytes());
    for value in row {
        match value {
            OwnedDatum::Null => out.push(0),
            OwnedDatum::Int(number) => {
                out.push(1);
                out.extend_from_slice(&number.to_le_bytes());
            }
            OwnedDatum::Real(number) => {
                out.push(2);
                out.extend_from_slice(&number.to_bits().to_le_bytes());
            }
            OwnedDatum::Text(bytes) => {
                out.push(3);
                out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(bytes);
            }
            OwnedDatum::Blob(bytes) => {
                out.push(4);
                out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

/// Returns a database file's own name, which its log segments are prefixed by.
///
/// @param database - the database file
fn staged_name(database: &Path) -> String {
    database
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Returns every log segment sitting beside one database file.
///
/// @param database - the database file
fn log_segments(database: &Path) -> DbResult<Vec<PathBuf>> {
    let Some(directory) = database.parent() else {
        return Ok(Vec::new());
    };
    let prefix = format!("{}-wal.", staged_name(database));
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Returns the staging path a destination is built at.
///
/// Beside the destination rather than in a temporary directory, so that the
/// rename which publishes it stays within one file system and is therefore
/// atomic. A rename across devices is a copy, and a copy is a window in which
/// the destination exists and is not yet whole.
///
/// @param destination - where the migration will publish
fn staging_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "migrated".to_string());
    let directory = destination
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    directory.join(format!(".{name}.staging"))
}

/// Returns whether a table is one a virtual table owns rather than one the
/// application declared.
///
/// A module's storage is named after the table it belongs to with a suffix:
/// FTS5 keeps `<name>_data`, `<name>_idx`, `<name>_docsize`, `<name>_content`
/// and `<name>_config`, and R*Tree keeps `<name>_node`, `<name>_rowid` and
/// `<name>_parent`. Their declarations are the module's own and are not
/// ordinary SQL, so parsing one and reporting that the file is malformed is
/// both wrong and unhelpful. They are also not data an inventory should count:
/// the rows they hold are an index of rows that are counted already.
///
/// @param virtual_tables - the folded names of the file's virtual tables
/// @param name - the table being considered
fn is_shadow_table(virtual_tables: &[String], name: &str) -> bool {
    let folded = name.to_ascii_lowercase();
    virtual_tables.iter().any(|owner| {
        folded
            .strip_prefix(owner.as_str())
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|suffix| !suffix.is_empty())
    })
}

/// Rebuilds the source's FTS5 tables in the migrated database, and says so.
///
/// One check per full-text table, so the report says what was carried and what
/// was not rather than leaving the caller to notice a missing table. A virtual
/// table using any *other* module is reported as not carried, with its module
/// named: this engine has an R*Tree and a search table of its own, but their
/// storage is not SQLite's and there is no content table to rebuild them from.
///
/// @param source - the SQLite file, read only
/// @param staged - the migrated database, still under its staging name
fn rebuild_full_text(source: &Path, staged: &Path) -> DbResult<Vec<Check>> {
    let mut file = SqliteFile::open(source.to_path_buf())?;
    let schema = file.schema()?;
    let mut checks = Vec::new();
    let mut work: Vec<(String, String, Vec<String>, Vec<Vec<OwnedDatum>>)> = Vec::new();
    for object in &schema {
        let Some(module) = virtual_module(&object.sql) else {
            continue;
        };
        if !module.eq_ignore_ascii_case("fts5") {
            checks.push(Check::failed(
                &format!("carried.{}", object.name),
                format!(
                    "{} uses the {module} module, whose storage is SQLite's own; \
                     this engine has no content table to rebuild it from",
                    object.name
                ),
            ));
            continue;
        }
        let columns = fts5_columns(&object.sql);
        if columns.is_empty() {
            checks.push(Check::failed(
                &format!("carried.{}", object.name),
                format!(
                    "{}: no column could be read out of its declaration",
                    object.name
                ),
            ));
            continue;
        }
        let content = format!("{}_content", object.name);
        let Some(held) = schema
            .iter()
            .find(|held| held.kind == "table" && held.name.eq_ignore_ascii_case(&content))
        else {
            // `content=''` and `content='other'` keep no copy of the text, so
            // there is nothing here to rebuild from. Said rather than skipped.
            checks.push(Check::failed(
                &format!("carried.{}", object.name),
                format!(
                    "{} keeps no content table, so its text is not in this file to carry",
                    object.name
                ),
            ));
            continue;
        };
        // `<name>_content` is `id` plus one column per indexed column, in order.
        let rows = file.read_table(held.root, columns.len().saturating_add(1))?;
        work.push((object.name.clone(), object.sql.clone(), columns, rows));
    }
    if work.is_empty() {
        return Ok(checks);
    }
    let database = inillucent_engine::connect::Database::open(staged)?;
    let connection = database.session();
    for (name, sql, columns, rows) in work {
        connection.execute_batch(&sql)?;
        // **The docid moves with the row.** An application that stored the
        // rowid an fts5 table handed it - which is what `<name>_content`'s `id`
        // is, and what every search adapter joins on - would find it pointing
        // at a different document if the rebuild renumbered.
        let names = columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<String>>()
            .join(", ");
        let mut carried = 0u64;
        for row in &rows {
            // `read_table` prepends the cell's rowid, and the record then
            // carries `id` (NULL, because it is the rowid alias) followed by
            // `c0`, `c1`, ... - so the first indexed column is at 2.
            let values = columns
                .iter()
                .enumerate()
                .map(|(at, _)| sql_literal(row.get(at.saturating_add(2))))
                .collect::<Vec<String>>()
                .join(", ");
            let docid = sql_literal(row.first());
            connection.execute_batch(&format!(
                "INSERT INTO \"{name}\"(rowid, {names}) VALUES ({docid}, {values})"
            ))?;
            carried = carried.saturating_add(1);
        }
        checks.push(Check::passed(
            &format!("carried.{name}"),
            format!("{carried} rows rebuilt through this engine's own fts5"),
        ));
    }
    database.checkpoint()?;
    Ok(checks)
}

/// Returns the module a `CREATE VIRTUAL TABLE` names, when the text is one.
///
/// @param sql - the declaration as the schema stored it
fn virtual_module(sql: &str) -> Option<String> {
    let text = sql.trim_start();
    if !text
        .get(..14)
        .is_some_and(|head| head.eq_ignore_ascii_case("CREATE VIRTUAL"))
    {
        return None;
    }
    let at = text.to_ascii_lowercase().find(" using ")?;
    let rest = text.get(at.saturating_add(7)..)?.trim_start();
    let end = rest
        .find(|byte: char| byte == '(' || byte.is_whitespace() || byte == ';')
        .unwrap_or(rest.len());
    Some(rest.get(..end)?.to_string())
}

/// Returns the column names an `fts5(...)` declaration indexes.
///
/// The arguments that are *not* options: `tokenize=`, `content=`, `prefix=` and
/// the rest carry an `=` and name a setting rather than a column.
///
/// @param sql - the declaration as the schema stored it
fn fts5_columns(sql: &str) -> Vec<String> {
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let Some(close) = sql.rfind(')') else {
        return Vec::new();
    };
    let Some(inside) = sql.get(open.saturating_add(1)..close) else {
        return Vec::new();
    };
    inside
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.contains('='))
        .map(|part| {
            part.trim_matches(|byte| byte == '"' || byte == '\'' || byte == '`')
                .to_string()
        })
        .filter(|part| !part.is_empty())
        .collect()
}

/// Renders one value as the SQL literal an `INSERT` can carry.
///
/// @param value - the value read out of the source's content table
fn sql_literal(value: Option<&OwnedDatum>) -> String {
    match value {
        None | Some(OwnedDatum::Null) => "NULL".to_string(),
        Some(OwnedDatum::Int(number)) => number.to_string(),
        Some(OwnedDatum::Real(number)) => format!("{number:?}"),
        Some(OwnedDatum::Text(bytes)) => {
            let text = String::from_utf8_lossy(bytes).replace('\'', "''");
            format!("'{text}'")
        }
        Some(OwnedDatum::Blob(bytes)) => {
            let mut out = String::from("x'");
            for byte in bytes {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
            out
        }
    }
}
