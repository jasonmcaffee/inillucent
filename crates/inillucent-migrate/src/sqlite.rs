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
    /// An ordered digest of every row, over the columns `digested` names.
    pub digest: String,
    /// Every column the source declares, in declared order.
    pub columns: Vec<String>,
    /// The declared positions the digest folds, which are the stored ones.
    ///
    /// **A `VIRTUAL` generated column is not in this list, and leaving it out
    /// is the check being true rather than the check being weakened
    /// (task-2050).** Such a column occupies no field in SQLite's record and no
    /// column in one of this engine's trees: it is computed on read, by
    /// whichever engine is doing the reading. So a migration copies nothing for
    /// it, and a digest that folded it would not be comparing a copy - it would
    /// be comparing two engines' evaluation of an expression, which this tool
    /// cannot do because it has no SQLite evaluator and is not allowed to
    /// require a SQLite installation.
    ///
    /// Before this list existed the source side folded `NULL` for every
    /// `VIRTUAL` column - `SourceLayout::slots` says `None` for one, meaning
    /// the tree does not carry it - while the destination side read `SELECT *`
    /// and folded the value the expression produced. Every database with a
    /// `VIRTUAL` generated column in it therefore failed `digest.<table>` with
    /// a staging file holding exactly the right rows, and was deleted.
    ///
    /// What the narrower digest gives up is caught by two things beside it: the
    /// `columns.<table>` check below compares the destination's whole column
    /// list against the source's, so a dropped or reordered generated column is
    /// still a failure; and
    /// `inillucent-compat::migrate_realistic::every_realistic_fixture_migrates_value_for_value`
    /// compares `SELECT *` against the pinned SQLite shell, which does evaluate
    /// both sides' expressions against a third engine.
    pub digested: Vec<usize>,
}

impl TableInventory {
    /// Returns the names of the columns the digest folds, in digest order.
    pub fn digested_columns(&self) -> Vec<String> {
        self.digested
            .iter()
            .map(|at| {
                self.columns
                    .get(*at)
                    .cloned()
                    .unwrap_or_else(|| format!("column {at}"))
            })
            .collect()
    }
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
    /// What `PRAGMA user_version` answers on the source.
    ///
    /// Carried because an application uses it to know which schema version it
    /// is looking at, and a migration that resets it to zero makes every
    /// migration step run again (task-2066 §4.1.7).
    pub user_version: u32,
    /// What `PRAGMA application_id` answers on the source.
    ///
    /// Carried for the same reason: it says which application owns the file,
    /// and a zero says "no application does".
    pub application_id: u32,
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

/// Returns `(user_version, application_id)` out of a SQLite file's header.
///
/// Read from the bytes rather than asked as a pragma, because the header is the
/// first hundred bytes of every SQLite file and its layout is part of the
/// format: `user_version` is a big-endian `u32` at offset 60 and
/// `application_id` is one at offset 68. Opening a second connection to ask
/// would be a second reader of the same file for two numbers.
///
/// `(0, 0)` for a file too short to have a header, which is the same answer
/// SQLite gives for a database that never set either - and a file that short is
/// refused by the inventory a few lines later anyway, with a better message
/// than this could give.
///
/// @param path - the SQLite file
fn header_versions(path: &Path) -> (u32, u32) {
    /// Where `user_version` sits in the header.
    const USER_VERSION_AT: usize = 60;
    /// Where `application_id` sits.
    const APPLICATION_ID_AT: usize = 68;

    let Ok(head) = std::fs::read(path) else {
        return (0, 0);
    };
    let read = |at: usize| -> u32 {
        head.get(at..at.saturating_add(4))
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map_or(0, u32::from_be_bytes)
    };
    (read(USER_VERSION_AT), read(APPLICATION_ID_AT))
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
    let (user_version, application_id) = header_versions(path);
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
        let (table, _rows) = read_one_table(path, &mut file, &object)?;
        tables.push(table);
    }
    Ok(SqliteInventory {
        path: path.to_path_buf(),
        page_size,
        page_count,
        user_version,
        application_id,
        tables,
    })
}

/// Reads one table's rows and returns what the inventory records about it.
///
/// Returns the rows beside the inventory because two callers need them and
/// neither keeps them: `inventory` folds them into a digest and drops them,
/// which is what stops a migration of a large database holding every row of it
/// in memory, and `describe_digest_difference` asks for one table's rows again
/// after that table's digest has already failed.
///
/// @param path - the source file, named in every failure
/// @param file - the opened source, read only
/// @param object - the table's row of the source's schema
fn read_one_table(
    path: &Path,
    file: &mut SqliteFile,
    object: &inillucent_sqlite_reader::SchemaObject,
) -> DbResult<(TableInventory, Vec<Vec<OwnedDatum>>)> {
    // The declaration, parsed by the same loader the import uses, because the
    // transform below is only right if both sides agree about which column is
    // the rowid alias and what order a `WITHOUT ROWID` table's record is in.
    let info = table_from_create_sql(object.sql.as_bytes(), 0, object.root).map_err(|error| {
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
    // A `WITHOUT ROWID` table's pages are index pages, so its rows are read as
    // index entries rather than as table rows.
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
    // destination is read with `SELECT *`, so digesting the storage shape here
    // would compare a row of six values against a row of five and call a
    // correct migration wrong.
    let rows: Vec<Vec<OwnedDatum>> = stored
        .iter()
        .map(|row| logical_row(&info, &layout, row))
        .collect();
    // The declared positions the file stores. `SourceLayout::slots` is indexed
    // by declared position and holds `None` for exactly the `VIRTUAL` generated
    // columns, which is the one thing that separates a column this migration
    // copied from a column both engines compute; `TableInventory::digested`
    // says why the digest folds only the first kind.
    let digested: Vec<usize> = (0..info.columns.len())
        .filter(|declared| layout.slots.get(*declared).copied().flatten().is_some())
        .collect();
    let columns: Vec<String> = info
        .columns
        .iter()
        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
        .collect();
    let digest = digest_rows(&narrow(&rows, &digested));
    Ok((
        TableInventory {
            name: object.name.clone(),
            rows: rows.len() as u64,
            digest,
            columns,
            digested,
        },
        rows,
    ))
}

/// Reads one named table's rows out of a source file again.
///
/// Only the failure path calls this: a digest that did not match is worth one
/// more read of one table, because the alternative is a refusal that names two
/// hashes and no row.
///
/// @param path - the source file
/// @param name - the table to read, compared case-insensitively
fn reread_table(path: &Path, name: &str) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let mut file = SqliteFile::open(path.to_path_buf())?;
    let schema = file.schema()?;
    let folded = name.to_ascii_lowercase();
    let Some(object) = schema
        .iter()
        .find(|object| {
            object.kind == "table" && object.root != 0 && object.name.to_ascii_lowercase() == folded
        })
        .cloned()
    else {
        return Ok(Vec::new());
    };
    let (_table, rows) = read_one_table(path, &mut file, &object)?;
    Ok(rows)
}

/// Returns every row cut down to the declared positions given.
///
/// @param rows - the rows, in declared order
/// @param wanted - the declared positions to keep, in order
fn narrow(rows: &[Vec<OwnedDatum>], wanted: &[usize]) -> Vec<Vec<OwnedDatum>> {
    rows.iter()
        .map(|row| {
            wanted
                .iter()
                .map(|at| row.get(*at).cloned().unwrap_or(OwnedDatum::Null))
                .collect()
        })
        .collect()
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

    // **The two schema pragmas move with the database** (task-2066 §4.1.7).
    // `application_id` says which application owns the file and `user_version`
    // says which schema version it is on; both were dropped, so a tool that
    // reads `user_version` to decide whether a migration step has run sees 0 on
    // a migrated database and runs every step again.
    //
    // Written before the verification below rather than after it, so the check
    // is reading the published file's own answer rather than the value this
    // function just decided to write.
    let pragmas = carry_schema_pragmas(&staged, &inventory)?;

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
    checks.extend(pragmas);
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
        let (rows, columns) = match built.run(&format!("SELECT * FROM \"{quoted}\"")) {
            Ok(answer) => answer,
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
        checks.push(column_check(table, &columns));
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
        let narrowed = narrow(&rows, &table.digested);
        let digest = digest_rows(&narrowed);
        if digest == table.digest {
            checks.push(Check::passed(&format!("digest.{name}"), digest));
        } else {
            checks.push(Check::failed(
                &format!("digest.{name}"),
                digest_failure(inventory, table, &narrowed),
            ));
        }
    }
    if inventory.tables.is_empty() {
        checks.push(Check::passed("tables", "the source holds none"));
    }
    checks
}

/// Checks that the migrated table declares the columns the source declared.
///
/// **This is what the narrowed digest hands over (task-2050).** The digest
/// folds only the stored columns, so a migration that dropped a `VIRTUAL`
/// generated column altogether, or that carried it as a plain column somewhere
/// else, would still digest identically. The column list is what says the table
/// is the same table, and it costs nothing: the names come back from the
/// `SELECT *` the digest already ran.
///
/// @param table - what the source held
/// @param produced - the column names the migrated table answered with
fn column_check(table: &TableInventory, produced: &[String]) -> Check {
    let name = &table.name;
    if produced == table.columns {
        return Check::passed(
            &format!("columns.{name}"),
            format!(
                "{} columns, in the order the source declares",
                produced.len()
            ),
        );
    }
    Check::failed(
        &format!("columns.{name}"),
        format!(
            "the source declares ({}) and the migration answers ({})",
            table.columns.join(", "),
            produced.join(", ")
        ),
    )
}

/// Returns what to say about a table whose two digests do not agree.
///
/// **A refusal names what it could not carry, and two hashes name nothing.**
/// `carried.document_fts` says "document_fts keeps no content table, so its
/// text is not in this file to carry", which is a sentence somebody can act on;
/// `digest.message 1c267ce8... migrated, 075d249b... in the source` is one
/// nobody can. So a digest that failed spends one more read of one table - the
/// migration is over either way - and reports the rows that differ.
///
/// @param inventory - the source, which is re-read for this table alone
/// @param table - the table whose digest failed
/// @param migrated - the migrated rows, already cut to the digested columns
fn digest_failure(
    inventory: &SqliteInventory,
    table: &TableInventory,
    migrated: &[Vec<OwnedDatum>],
) -> String {
    let source = match reread_table(&inventory.path, &table.name) {
        Ok(rows) => narrow(&rows, &table.digested),
        Err(error) => {
            return format!(
                "the values differ, and {} could not be read again to say which: {}",
                inventory.path.display(),
                error.message()
            )
        }
    };
    describe_digest_difference(&table.digested_columns(), &source, migrated)
}

/// How many differing rows a refusal prints from each side.
///
/// Enough to see a pattern - a shifted column shows in every row and a single
/// lost value in one - and few enough that the refusal still fits on a screen.
const DIFFERENCE_EXAMPLES: usize = 3;

/// Returns a sentence naming the rows two sides do not share.
///
/// The comparison is over multisets, which is the comparison the digest makes:
/// a row is matched by its encoded bytes, so a row that moved is not a
/// difference and a row whose value changed is two differences, one on each
/// side. Each side's leftovers are reported separately, because "in the source
/// and not in the migration" and "in the migration and not in the source" are
/// different defects - the first is a lost row and the second is a changed
/// value or an invented row.
///
/// @param columns - the digested columns' names, in digest order
/// @param source - the source's rows, cut to those columns
/// @param migrated - the migrated rows, cut to those columns
fn describe_digest_difference(
    columns: &[String],
    source: &[Vec<OwnedDatum>],
    migrated: &[Vec<OwnedDatum>],
) -> String {
    let missing = held_by_one_side(source, migrated);
    let extra = held_by_one_side(migrated, source);
    let over = if columns.is_empty() {
        "no columns".to_string()
    } else {
        format!("({})", columns.join(", "))
    };
    let mut out = format!(
        "{} rows in the source and {} in the migration, compared over {over}; ",
        source.len(),
        migrated.len()
    );
    if missing.is_empty() && extra.is_empty() {
        out.push_str("every row is in both, so the two digests were taken over different columns");
        return out;
    }
    out.push_str(&format!(
        "{} in the source and not in the migration{}",
        missing.len(),
        examples(columns, &missing)
    ));
    out.push_str(&format!(
        ", {} in the migration and not in the source{}",
        extra.len(),
        examples(columns, &extra)
    ));
    out
}

/// Returns the rows one side holds that the other does not, counting duplicates.
///
/// @param rows - the side being reported on
/// @param other - the side being compared against
fn held_by_one_side<'a>(
    rows: &'a [Vec<OwnedDatum>],
    other: &[Vec<OwnedDatum>],
) -> Vec<&'a Vec<OwnedDatum>> {
    let mut available: Vec<Vec<u8>> = other.iter().map(|row| encode_row(row)).collect();
    available.sort();
    let mut out = Vec::new();
    for row in rows {
        match available.binary_search(&encode_row(row)) {
            Ok(at) => {
                available.remove(at);
            }
            Err(_) => out.push(row),
        }
    }
    out
}

/// Returns up to `DIFFERENCE_EXAMPLES` rows, rendered, or nothing when there are none.
///
/// @param columns - the digested columns' names, in digest order
/// @param rows - the rows one side holds and the other does not
fn examples(columns: &[String], rows: &[&Vec<OwnedDatum>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = rows
        .iter()
        .take(DIFFERENCE_EXAMPLES)
        .map(|row| render_row(columns, row))
        .collect();
    let more = rows.len().saturating_sub(shown.len());
    let tail = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!(" - {}{tail}", shown.join("; "))
}

/// How much of one value a rendered row shows.
///
/// A two hundred kilobyte blob printed in full is a refusal nobody reads, and
/// the first characters plus the length say what a person needs in order to
/// tell one value from another.
const RENDERED_VALUE_CHARS: usize = 24;

/// Returns one row as `name=value` pairs a person can read.
///
/// @param columns - the digested columns' names, in digest order
/// @param row - the row's values, in the same order
fn render_row(columns: &[String], row: &[OwnedDatum]) -> String {
    let rendered: Vec<String> = row
        .iter()
        .enumerate()
        .map(|(at, value)| {
            let name = columns
                .get(at)
                .cloned()
                .unwrap_or_else(|| format!("column {at}"));
            format!("{name}={}", render_value(value))
        })
        .collect();
    format!("({})", rendered.join(", "))
}

/// Returns one value, shortened, with its type visible in how it is written.
///
/// @param value - the value
fn render_value(value: &OwnedDatum) -> String {
    match value {
        OwnedDatum::Null => "NULL".to_string(),
        OwnedDatum::Int(number) => number.to_string(),
        OwnedDatum::Real(number) => {
            String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(*number)).into_owned()
        }
        OwnedDatum::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            match text.char_indices().nth(RENDERED_VALUE_CHARS) {
                Some((at, _)) => format!(
                    "'{}...' ({} bytes)",
                    text.get(..at).unwrap_or_default().replace(QUOTE, "''"),
                    bytes.len()
                ),
                None => format!("'{}'", text.replace(QUOTE, "''")),
            }
        }
        OwnedDatum::Blob(bytes) => {
            let head: String = bytes
                .iter()
                .take(RENDERED_VALUE_CHARS / 2)
                .map(|byte| format!("{byte:02x}"))
                .collect();
            if bytes.len() > RENDERED_VALUE_CHARS / 2 {
                format!("x'{head}...' ({} bytes)", bytes.len())
            } else {
                format!("x'{head}'")
            }
        }
    }
}

/// The quote a rendered text value is wrapped in, and therefore doubles.
const QUOTE: char = '\'';

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

/// Writes the source's `application_id` and `user_version` onto the staged file.
///
/// Returns a check per pragma, so a value that did not survive the write is a
/// named failure rather than a silent zero. The check reads the staged database
/// back rather than trusting the write, which is the same argument the row
/// verification makes one level up: what is graded is what the file says.
///
/// @param staged - the database being built
/// @param inventory - what the source held
fn carry_schema_pragmas(staged: &Path, inventory: &SqliteInventory) -> DbResult<Vec<Check>> {
    let database = inillucent_engine::connect::Database::open(staged)?;
    let connection = database.session();
    connection.execute_batch(&format!(
        "PRAGMA application_id = {}; PRAGMA user_version = {};",
        inventory.application_id, inventory.user_version
    ))?;
    let mut checks = Vec::new();
    for (name, wanted) in [
        ("application_id", inventory.application_id),
        ("user_version", inventory.user_version),
    ] {
        let rows = connection.query(&format!("PRAGMA {name}"))?;
        let found = rows
            .first()
            .and_then(|row| row.first())
            .and_then(|value| match value {
                OwnedDatum::Int(number) => u32::try_from(*number).ok(),
                _ => None,
            })
            .unwrap_or(0);
        checks.push(if found == wanted {
            Check::passed(
                &format!("pragma.{name}"),
                format!("{wanted} carried from the source"),
            )
        } else {
            Check::failed(
                &format!("pragma.{name}"),
                format!("the source says {wanted} and the destination says {found}"),
            )
        });
    }
    Ok(checks)
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

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_catalog::load::index_from_create_sql;
    use inillucent_value::Affinity;

    /// The rowid the reader hands back for a row it read out of a table page.
    const ROWID: i64 = 7;

    /// Every declared type maps to the affinity SQLite's substring rules give.
    ///
    /// **A migration that got this wrong would publish a file that compares
    /// differently rather than one that reads differently**, which is the worse
    /// half: `WHERE price = '10'` is true in a NUMERIC column and false in a
    /// TEXT one, so the rows all arrive and the answers change. The affinity is
    /// what `logical_row` reads to decide the physical type a value is stored
    /// as, so it is decided here, once, for the inventory and for the import
    /// both.
    ///
    /// The last three rows are the ones a lookup table of known type names gets
    /// wrong: `POINT` is an integer column because it contains `INT`, `STRING`
    /// is numeric because it contains none of the five substrings, and
    /// `VARCHAR(20)` is text because it contains `CHAR`.
    #[test]
    fn every_declared_type_maps_to_the_affinity_sqlites_substring_rules_give() {
        let sql = "CREATE TABLE t (
            a INTEGER, b INT, c VARCHAR(20), d TEXT, e BLOB, f,
            g REAL, h DOUBLE PRECISION, i FLOAT, j NUMERIC,
            k DECIMAL(10,5), l BOOLEAN, m DATETIME, n POINT, o STRING
        )";
        let info = table_from_create_sql(sql.as_bytes(), 0, 2).expect("the declaration parses");
        let wanted = [
            ("a", Affinity::Integer),
            ("b", Affinity::Integer),
            ("c", Affinity::Text),
            ("d", Affinity::Text),
            ("e", Affinity::Blob),
            ("f", Affinity::Blob),
            ("g", Affinity::Real),
            ("h", Affinity::Real),
            ("i", Affinity::Real),
            ("j", Affinity::Numeric),
            ("k", Affinity::Numeric),
            ("l", Affinity::Numeric),
            ("m", Affinity::Numeric),
            ("n", Affinity::Integer),
            ("o", Affinity::Numeric),
        ];
        assert_eq!(
            info.columns.len(),
            wanted.len(),
            "the table declares {} columns and this case names {}",
            info.columns.len(),
            wanted.len()
        );
        for (column, (name, affinity)) in info.columns.iter().zip(wanted.iter()) {
            assert_eq!(column.name, name.as_bytes(), "the columns are out of order");
            assert_eq!(
                column.affinity,
                *affinity,
                "{name} is declared {} and came out {:?}",
                String::from_utf8_lossy(&column.declared_type),
                column.affinity
            );
        }
    }

    /// The declared type is carried across as it was written, not normalised.
    ///
    /// The affinity above is what the engine *uses*; the text is what
    /// `PRAGMA table_info` and a reconstructed `CREATE TABLE` show. An
    /// application that reads its own schema back to build a form or a
    /// migration of its own sees `VARCHAR(20)`, so a migration that rewrote it
    /// as `TEXT` would be a lossy copy of something nobody asked it to change.
    #[test]
    fn a_declared_type_is_carried_across_exactly_as_it_was_written() {
        let sql = "CREATE TABLE t (a VARCHAR(20), b DECIMAL(10,5), c DOUBLE PRECISION, d)";
        let info = table_from_create_sql(sql.as_bytes(), 0, 2).expect("the declaration parses");
        let written: Vec<String> = info
            .columns
            .iter()
            .map(|column| String::from_utf8_lossy(&column.declared_type).to_string())
            .collect();
        assert_eq!(
            written,
            vec![
                "VARCHAR(20)".to_string(),
                "DECIMAL(10,5)".to_string(),
                "DOUBLE PRECISION".to_string(),
                String::new(),
            ]
        );
    }

    /// `INTEGER PRIMARY KEY` is the rowid alias; `INT PRIMARY KEY` is not.
    ///
    /// **The one-word difference decides where the value is stored**, and a
    /// migration that read it wrong writes NULL into every row of the column
    /// people join on. An alias column's value is the cell key and its record
    /// field is empty; an `INT PRIMARY KEY` is an ordinary column with a unique
    /// index over it, and its value is in the record like any other.
    #[test]
    fn an_integer_primary_key_is_the_rowid_alias_and_an_int_one_is_not() {
        let alias =
            table_from_create_sql(b"CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", 0, 2)
                .expect("the declaration parses");
        assert_eq!(alias.rowid_alias, Some(0));

        let ordinary =
            table_from_create_sql(b"CREATE TABLE u (id INT PRIMARY KEY, body TEXT)", 0, 3)
                .expect("the declaration parses");
        assert_eq!(ordinary.rowid_alias, None);
        assert_eq!(
            ordinary.columns.first().map(|column| column.affinity),
            Some(Affinity::Integer),
            "an INT PRIMARY KEY is still an integer column; it is only not the rowid"
        );
    }

    /// The rowid alias comes back in the position it was declared in.
    ///
    /// This is the transform `inventory` digests through, and it is the reason
    /// the digest is comparable at all: the reader hands back
    /// `[rowid] ++ record fields` with the alias field empty, and a `SELECT *`
    /// against the migrated database produces the declared order with the rowid
    /// in the middle. Digesting the storage shape would compare a row of four
    /// values against a row of three and call a correct migration wrong.
    #[test]
    fn the_rowid_alias_comes_back_in_the_position_it_was_declared_in() {
        let info = table_from_create_sql(
            b"CREATE TABLE t (body TEXT, id INTEGER PRIMARY KEY, tag TEXT)",
            0,
            2,
        )
        .expect("the declaration parses");
        let layout = source_layout_of(&info).expect("the shape derives");
        let stored = vec![
            OwnedDatum::Int(ROWID),
            OwnedDatum::Text(b"a body".to_vec()),
            OwnedDatum::Null,
            OwnedDatum::Text(b"a tag".to_vec()),
        ];
        assert_eq!(
            logical_row(&info, &layout, &stored),
            vec![
                OwnedDatum::Text(b"a body".to_vec()),
                OwnedDatum::Int(ROWID),
                OwnedDatum::Text(b"a tag".to_vec()),
            ]
        );
    }

    /// An integer stored in a REAL column is read back as a real.
    ///
    /// SQLite stores 7 in a REAL column as 7.0, and the digest is taken over
    /// what a query sees rather than over what the page holds - so the
    /// conversion has to happen on this side too. Without it the source digests
    /// an integer, the migrated database digests a real, and the migration is
    /// refused for carrying the value it was given.
    #[test]
    fn an_integer_in_a_real_column_is_read_back_as_a_real() {
        let info = table_from_create_sql(
            b"CREATE TABLE t (id INTEGER PRIMARY KEY, weight REAL, label TEXT)",
            0,
            2,
        )
        .expect("the declaration parses");
        let layout = source_layout_of(&info).expect("the shape derives");
        let stored = vec![
            OwnedDatum::Int(ROWID),
            OwnedDatum::Null,
            OwnedDatum::Int(7),
            OwnedDatum::Text(b"kg".to_vec()),
        ];
        assert_eq!(
            logical_row(&info, &layout, &stored),
            vec![
                OwnedDatum::Int(ROWID),
                OwnedDatum::Real(7.0),
                OwnedDatum::Text(b"kg".to_vec()),
            ]
        );
    }

    /// An index key carries the direction it was declared with, on both flags.
    ///
    /// **Two flags, because two different pieces of code ask the question.**
    /// `descending` is what the tree is built with - `rowshape.rs` turns it
    /// into a descending key column - and `declared_descending` is what
    /// `PRAGMA index_xinfo` reports, which is how an application reconstructs
    /// its own `CREATE INDEX` after a migration. The parse sets both from the
    /// declaration; they only differ where a later pass flattens the storage
    /// side, and this is the pass a migration reads.
    ///
    /// A migration that dropped the `DESC` would publish an index a keyset read
    /// walks backwards. `compat/fixtures/realistic/browser-history.sql` carries
    /// such an index, and `migrate_realistic.rs` asks the same question of a
    /// published database.
    #[test]
    fn an_index_key_carries_the_direction_it_was_declared_with() {
        let table = table_from_create_sql(
            b"CREATE TABLE visit (id INTEGER PRIMARY KEY, place_id INTEGER, at INTEGER, note TEXT)",
            0,
            2,
        )
        .expect("the declaration parses");
        let index = index_from_create_sql(
            b"CREATE INDEX visit_when_idx ON visit (place_id, at DESC, note ASC)",
            &table,
            3,
        )
        .expect("the index parses");
        let declared: Vec<bool> = index
            .columns
            .iter()
            .map(|key| key.declared_descending)
            .collect();
        assert_eq!(declared, vec![false, true, false]);
        let stored: Vec<bool> = index.columns.iter().map(|key| key.descending).collect();
        assert_eq!(
            stored, declared,
            "the storage flag and the reported flag come out of one declaration"
        );
    }

    /// A column's collation is carried, and an index key inherits it.
    ///
    /// A migration that dropped `COLLATE NOCASE` would publish a database whose
    /// unique index accepts a row the source refused, and whose `ORDER BY` puts
    /// `Zebra` before `apple`. The collation is on the column, so an index over
    /// that column takes it without naming it.
    ///
    /// A column that declares none is `binary`, written out rather than left
    /// empty, so every key has a collation name and nothing downstream has to
    /// decide what an absent one means.
    #[test]
    fn a_column_collation_is_carried_and_an_index_key_inherits_it() {
        let table = table_from_create_sql(
            b"CREATE TABLE place (id INTEGER PRIMARY KEY, host TEXT COLLATE NOCASE, title TEXT)",
            0,
            2,
        )
        .expect("the declaration parses");
        let collations: Vec<String> = table
            .columns
            .iter()
            .map(|column| String::from_utf8_lossy(&column.collation).to_string())
            .collect();
        assert_eq!(
            collations,
            vec![
                "binary".to_string(),
                "nocase".to_string(),
                "binary".to_string(),
            ],
            "a column that declares no collation is BINARY, written out rather than left empty"
        );

        let index =
            index_from_create_sql(b"CREATE INDEX place_host_idx ON place (host)", &table, 3)
                .expect("the index parses");
        let key = index.columns.first().expect("one key column");
        assert_eq!(
            String::from_utf8_lossy(&key.collation),
            "nocase",
            "an index over a NOCASE column is ordered by NOCASE without saying so"
        );
    }

    /// An index key may name a collation of its own, and it wins.
    ///
    /// The other half of the question above: `CREATE INDEX ... (host COLLATE
    /// BINARY)` over a `COLLATE NOCASE` column is an index ordered by BINARY,
    /// and it is a different index from the one that inherits. Carrying the
    /// column's collation into every key would silently merge the two.
    #[test]
    fn an_index_key_may_name_a_collation_of_its_own() {
        let table = table_from_create_sql(
            b"CREATE TABLE place (id INTEGER PRIMARY KEY, host TEXT COLLATE NOCASE)",
            0,
            2,
        )
        .expect("the declaration parses");
        let index = index_from_create_sql(
            b"CREATE INDEX place_host_binary ON place (host COLLATE BINARY)",
            &table,
            3,
        )
        .expect("the index parses");
        let key = index.columns.first().expect("one key column");
        assert_eq!(String::from_utf8_lossy(&key.collation), "binary");
    }

    /// A unique partial index keeps both halves of what it was declared with.
    ///
    /// `browser-history` carries one, and the two properties fail differently:
    /// dropping `UNIQUE` publishes a database that accepts a duplicate the
    /// source refused, and dropping the `WHERE` publishes one that refuses a
    /// duplicate the source accepted.
    #[test]
    fn a_unique_partial_index_keeps_both_halves_of_its_declaration() {
        let table = table_from_create_sql(
            b"CREATE TABLE place (id INTEGER PRIMARY KEY, url TEXT, archived INTEGER)",
            0,
            2,
        )
        .expect("the declaration parses");
        let index = index_from_create_sql(
            b"CREATE UNIQUE INDEX place_live_url ON place (url) WHERE archived = 0",
            &table,
            3,
        )
        .expect("the index parses");
        assert!(index.unique, "the declaration said UNIQUE");
        let predicate = index
            .partial_sql
            .as_ref()
            .map(|bytes| String::from_utf8_lossy(bytes).to_string())
            .unwrap_or_default();
        assert!(
            predicate.contains("archived") && predicate.contains('0'),
            "the partial predicate came back as {predicate:?}"
        );
    }

    /// Returns a table inventory over the columns and digested positions given.
    ///
    /// @param columns - the declared column names
    /// @param digested - the declared positions the digest folds
    fn inventory_of(columns: &[&str], digested: &[usize]) -> TableInventory {
        TableInventory {
            name: "t".to_string(),
            rows: 0,
            digest: String::new(),
            columns: columns.iter().map(|name| name.to_string()).collect(),
            digested: digested.to_vec(),
        }
    }

    /// A `VIRTUAL` generated column is left out of the digest and a `STORED` one is not.
    ///
    /// **This is task-2050's defect, stated as the shape the inventory has.**
    /// `SourceLayout::slots` holds `None` for exactly the columns the tree does
    /// not carry, and `VIRTUAL` is the only kind of column that is declared and
    /// not carried. Before this, every declared position was folded and the
    /// `VIRTUAL` ones were folded as `NULL` - a value the destination never
    /// produces, because it evaluates the expression - so every database with
    /// one in it failed its own verification.
    #[test]
    fn a_virtual_generated_column_is_not_one_of_the_digested_positions() {
        let info = table_from_create_sql(
            b"CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT, \
              v INTEGER GENERATED ALWAYS AS (length(b)) VIRTUAL, \
              s TEXT GENERATED ALWAYS AS (upper(b)) STORED)",
            0,
            2,
        )
        .expect("the declaration parses");
        let layout = source_layout_of(&info).expect("the shape derives");
        let digested: Vec<usize> = (0..info.columns.len())
            .filter(|declared| layout.slots.get(*declared).copied().flatten().is_some())
            .collect();
        assert_eq!(
            digested,
            vec![0, 1, 3],
            "a, b and the STORED column are carried in the file; the VIRTUAL one is not"
        );
    }

    /// The column check compares the whole declared list, not the digested part.
    ///
    /// The digest no longer sees a `VIRTUAL` generated column, so this is what
    /// notices a migration that dropped one or put it somewhere else.
    #[test]
    fn the_column_check_fails_when_a_generated_column_is_missing_from_the_migration() {
        let table = inventory_of(&["a", "b", "v", "s"], &[0, 1, 3]);
        let whole = [
            "a".to_string(),
            "b".to_string(),
            "v".to_string(),
            "s".to_string(),
        ];
        let passed = column_check(&table, &whole);
        assert!(passed.passed, "the same four columns in the same order");
        assert_eq!(passed.name, "columns.t");

        let dropped = ["a".to_string(), "b".to_string(), "s".to_string()];
        let failed = column_check(&table, &dropped);
        assert!(
            !failed.passed,
            "the migration answers three of four columns"
        );
        assert_eq!(
            failed.detail,
            "the source declares (a, b, v, s) and the migration answers (a, b, s)"
        );
    }

    /// A digest failure names the rows that differ rather than two hashes.
    ///
    /// **The refusal a person can act on.** `carried.document_fts` says why it
    /// could not carry an external content index; a digest that said
    /// `1c267ce8... migrated, 075d249b... in the source` said nothing at all,
    /// which is the second half of task-2050.
    #[test]
    fn a_digest_difference_names_the_rows_each_side_holds_alone() {
        let columns = ["id".to_string(), "body".to_string()];
        let source = vec![
            vec![OwnedDatum::Int(1), OwnedDatum::Text(b"one".to_vec())],
            vec![OwnedDatum::Int(2), OwnedDatum::Text(b"two".to_vec())],
        ];
        let migrated = vec![
            vec![OwnedDatum::Int(1), OwnedDatum::Text(b"one".to_vec())],
            vec![OwnedDatum::Int(2), OwnedDatum::Text(b"TWO".to_vec())],
        ];
        assert_eq!(
            describe_digest_difference(&columns, &source, &migrated),
            "2 rows in the source and 2 in the migration, compared over (id, body); \
             1 in the source and not in the migration - (id=2, body='two'), \
             1 in the migration and not in the source - (id=2, body='TWO')"
        );
    }

    /// A row missing from the migration is reported as missing, and only once.
    ///
    /// Rule 1.1: the two directions are different defects, and a case that only
    /// exercised a changed value would pass with the two halves swapped.
    #[test]
    fn a_lost_row_is_reported_on_the_side_that_lost_it() {
        let columns = ["id".to_string()];
        let source = vec![
            vec![OwnedDatum::Int(1)],
            vec![OwnedDatum::Int(2)],
            vec![OwnedDatum::Int(3)],
        ];
        let migrated = vec![vec![OwnedDatum::Int(1)], vec![OwnedDatum::Int(3)]];
        assert_eq!(
            describe_digest_difference(&columns, &source, &migrated),
            "3 rows in the source and 2 in the migration, compared over (id); \
             1 in the source and not in the migration - (id=2), \
             0 in the migration and not in the source"
        );
    }

    /// Two sides holding the same rows and digesting differently is said plainly.
    ///
    /// It is not a data difference, so naming a row would be a lie. It is the
    /// two sides folding different columns, which is the defect task-2050 was.
    #[test]
    fn identical_rows_that_digested_differently_say_so() {
        let columns = ["id".to_string()];
        let rows = vec![vec![OwnedDatum::Int(1)]];
        assert_eq!(
            describe_digest_difference(&columns, &rows, &rows),
            "1 rows in the source and 1 in the migration, compared over (id); every row is in \
             both, so the two digests were taken over different columns"
        );
    }

    /// Only the first three differing rows are printed, and the rest are counted.
    #[test]
    fn a_wholly_different_table_prints_three_rows_and_counts_the_rest() {
        let columns = ["id".to_string()];
        let source: Vec<Vec<OwnedDatum>> = (1..=10).map(|n| vec![OwnedDatum::Int(n)]).collect();
        let migrated: Vec<Vec<OwnedDatum>> =
            (101..=110).map(|n| vec![OwnedDatum::Int(n)]).collect();
        let said = describe_digest_difference(&columns, &source, &migrated);
        assert!(
            said.contains("(id=1); (id=2); (id=3) and 7 more"),
            "three rows and a count of the rest: {said}"
        );
        assert!(
            said.contains("(id=101); (id=102); (id=103) and 7 more"),
            "and the same from the other side: {said}"
        );
    }

    /// A long text and a large blob are shortened, with their real length kept.
    ///
    /// A refusal carrying a two hundred kilobyte blob is one nobody reads, and
    /// `chat-archive.db` has blobs that size in it.
    #[test]
    fn a_rendered_value_is_shortened_and_keeps_its_length() {
        assert_eq!(render_value(&OwnedDatum::Null), "NULL");
        assert_eq!(render_value(&OwnedDatum::Int(-9)), "-9");
        assert_eq!(
            render_value(&OwnedDatum::Text(b"it's short".to_vec())),
            "'it''s short'"
        );
        assert_eq!(
            render_value(&OwnedDatum::Text(vec![b'x'; 100])),
            format!("'{}...' (100 bytes)", "x".repeat(24))
        );
        assert_eq!(
            render_value(&OwnedDatum::Blob(vec![0xab; 200_000])),
            format!("x'{}...' (200000 bytes)", "ab".repeat(12))
        );
    }
}
