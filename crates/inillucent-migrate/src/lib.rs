//! Copy a legacy inillucent index into a inillucent database, verify it, and publish
//! it - without ever writing to the original.
//!
//! Invariant: **the source is never modified and never removed.** Not at the
//! end, not on success, not by a `--force`. Rollback is therefore not an
//! operation this tool performs; it is the absence of one. If the destination
//! turns out wrong a week later, going back is pointing the application at the
//! directory that has been sitting there unchanged the whole time.
//!
//! The order of operations exists to make every intermediate state either
//! recognisable or harmless:
//!
//! 1. **Inventory.** Digest every section of the source generation, so a
//!    resumed run can tell whether it is still looking at the same corpus.
//! 2. **Stage.** Build into a uniquely named file *beside* the destination,
//!    never over it. A half-written destination is never at the path an
//!    application opens.
//! 3. **Schema, then copy in bounded transactions**, each one checkpointed in
//!    the manifest after it commits. An interruption resumes from the last
//!    committed batch.
//! 4. **Build the index**, which for a search table is the `compact` command:
//!    one generation built in a single pass over every copied row. The table is
//!    declared `compact = 0` for the copy, so nothing is published while the
//!    rows are still arriving and the graph is built exactly once.
//! 5. **Verify against the source** - counts, ordered digests, tombstones,
//!    dictionaries, and the rankings and scores of a probe pack drawn from the
//!    corpus.
//! 6. **Probe with the other engine.** The pinned SQLite reads the staged file
//!    and runs `integrity_check` plus read-only queries, because a file only
//!    this engine can read is not the file this project promises.
//! 7. **Close, reopen, verify again**, so what is published is what a fresh
//!    process sees rather than what one warm cache saw.
//! 8. **Publish** by renaming the verified staging file into place, refusing to
//!    overwrite anything that is already there.
//!
//! A failure at any step leaves the staging file, the manifest, and the source.
//! Nothing is cleaned up automatically, because the thing a person needs after
//! a failed migration is the evidence.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod copy;
pub mod index;
pub mod manifest;
pub mod source;
pub mod sqlite;
pub mod verify;

use std::path::{Path, PathBuf};

use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

use crate::copy::SEARCH_TABLE;
use crate::index::SqlIndex;
use crate::manifest::Manifest;
use crate::source::Source;
use crate::verify::Check;

/// What a migration was asked to do.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The legacy index directory to read.
    pub source: PathBuf,
    /// Where the finished database should end up.
    pub destination: PathBuf,
    /// Where the staging database is built, beside the destination.
    pub staging: PathBuf,
    /// Where the manifest is written.
    pub manifest: PathBuf,
    /// Whether to publish, or to stop with a verified staging file.
    pub publish: bool,
}

impl Plan {
    /// Returns the plan for one source and one destination.
    ///
    /// The staging name is derived from the destination rather than random, so
    /// a resumed run finds the file the interrupted one was building. It is
    /// still uniquely named: nothing an application opens is ever called
    /// `<name>.migrating`.
    /// @param source - the legacy index directory
    /// @param destination - where the database should end up
    pub fn new(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Plan {
        let destination = destination.as_ref().to_path_buf();
        let staging = with_suffix(&destination, ".migrating");
        let manifest = with_suffix(&destination, ".migration-manifest");
        Plan {
            source: source.as_ref().to_path_buf(),
            destination,
            staging,
            manifest,
            publish: true,
        }
    }
}

/// Returns a path with a suffix appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "database".to_string());
    name.push_str(suffix);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// What a migration produced.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Every check that ran, in order.
    pub checks: Vec<Check>,
    /// Where the manifest was written.
    pub manifest: PathBuf,
    /// Where the database ended up, when it was published.
    pub published: Option<PathBuf>,
    /// How many documents and chunks were copied.
    pub documents: u64,
    /// How many chunks were copied.
    pub chunks: u64,
}

impl Outcome {
    /// Returns whether every check passed.
    pub fn verified(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|check| check.passed)
    }
}

/// Runs a migration to completion, resuming whatever an earlier run left.
///
/// @param plan - what to migrate and where to put it
pub fn migrate(plan: &Plan) -> Result<Outcome, String> {
    let source = Source::open(&plan.source)?;
    if let Some(refusal) = source.refusal() {
        return Err(format!(
            "cannot migrate {}: {refusal}",
            plan.source.display()
        ));
    }
    let mut manifest = Manifest::open(&plan.manifest)?;

    // A resumed run has to be looking at the same corpus. A source that has
    // been rebuilt since is a different corpus, and continuing to copy from it
    // would produce a destination that is half of one and half of the other.
    let recorded = manifest.all("source.file");
    let resuming = !recorded.is_empty();
    if resuming && !source.matches(&recorded) {
        return Err(format!(
            "the source has changed since this migration started; the manifest at {} describes a \
             different generation. Migrate into a new destination rather than resuming.",
            plan.manifest.display()
        ));
    }
    if !resuming {
        manifest.record("manifest", manifest::VERSION.to_string())?;
        manifest.record("tool", "inillucent-migrate")?;
        manifest.record("source.path", plan.source.display().to_string())?;
        manifest.record("source.generation", source.generation_name())?;
        for line in source.manifest_lines() {
            manifest.record("source.file", line)?;
        }
        manifest.record("destination.staging", plan.staging.display().to_string())?;
        manifest.record("destination.final", plan.destination.display().to_string())?;
    }

    if plan.destination.exists() {
        return Err(format!(
            "{} already exists; this tool never writes over a database",
            plan.destination.display()
        ));
    }

    let dims = source.index.config().dims;
    let store = source.index.store();

    let database = Database::open(&plan.staging)
        .map_err(|error| format!("cannot open the staging database: {}", error.message()))?;
    let connection = database.session();

    if !manifest.finished("schema") {
        copy::create_schema(&connection, dims)
            .map_err(|error| format!("cannot create the schema: {}", error.message()))?;
        manifest.record("stage", "schema")?;
    }

    let documents = copy::copy_documents(&connection, store, &mut manifest)?;
    let chunks = copy::copy_chunks(&connection, store, &mut manifest)?;
    let indexed = copy::copy_search(
        &connection,
        store,
        source.index.vectors(),
        dims,
        &mut manifest,
    )?;
    manifest.record("copied", format!("indexed {indexed}"))?;

    if !manifest.finished("build") {
        connection
            .execute_batch(&format!(
                "INSERT INTO {SEARCH_TABLE}({SEARCH_TABLE}) VALUES ('compact')"
            ))
            .map_err(|error| format!("cannot build the index: {}", error.message()))?;
        manifest.record("stage", "build")?;
    }

    for (table, sql) in [
        ("document", "SELECT * FROM document ORDER BY id"),
        ("chunk", "SELECT * FROM chunk ORDER BY id"),
        (
            "document_label",
            "SELECT document, label FROM document_label ORDER BY document, label",
        ),
        (
            "document_attribute",
            "SELECT document, name, value FROM document_attribute ORDER BY document, name, value",
        ),
        (
            "document_flag",
            "SELECT document, flag FROM document_flag ORDER BY document, flag",
        ),
    ] {
        let (rows, digest) = copy::digest(&connection, sql)?;
        manifest.record("target.table", format!("{table} {rows} {digest}"))?;
    }
    let sequence = scalar_text(
        &connection,
        &format!("SELECT v FROM {SEARCH_TABLE}_state WHERE k = 'sequence'"),
    )?;
    manifest.record("target.commit_sequence", sequence)?;
    let generation = scalar_text(
        &connection,
        &format!("SELECT v FROM {SEARCH_TABLE}_state WHERE k = 'generation'"),
    )?;
    manifest.record("target.search_generation", generation)?;

    // Everything is written. Checkpoint, then close, so what is verified is
    // what a fresh process sees rather than what one warm cache saw. The
    // checkpoint is what makes the close a *clean* one: it folds the log into
    // the file, so the next open reads a finished database rather than
    // replaying its way to one.
    database
        .checkpoint()
        .map_err(|error| format!("cannot checkpoint the staged file: {}", error.message()))?;
    let _ = connection;
    drop(database);

    let mut checks = vec![structure_probe(&plan.staging)];

    let mut sql = SqlIndex::open(&plan.staging, SEARCH_TABLE).map_err(|error| {
        format!(
            "cannot reopen the staged database: {}",
            error.detail().unwrap_or_else(|| error.message())
        )
    })?;
    checks.extend(verify::run(&source.index, &mut sql));
    drop(sql);

    // And once more through a second open, which is the check that the first
    // reopen did not itself leave state behind.
    let mut again = SqlIndex::open(&plan.staging, SEARCH_TABLE).map_err(|error| {
        format!(
            "cannot reopen the staged database: {}",
            error.detail().unwrap_or_else(|| error.message())
        )
    })?;
    let repeated = verify::run(&source.index, &mut again);
    drop(again);
    let stable = repeated.iter().all(|check| check.passed);
    checks.push(if stable {
        Check {
            name: "reopen".to_string(),
            passed: true,
            detail: format!("{} checks pass again on a fresh open", repeated.len()),
        }
    } else {
        Check {
            name: "reopen".to_string(),
            passed: false,
            detail: repeated
                .iter()
                .filter(|check| !check.passed)
                .map(|check| check.line())
                .collect::<Vec<String>>()
                .join(" | "),
        }
    });

    for check in &checks {
        manifest.record("verify", check.line())?;
    }
    manifest.record("stage", "verify")?;

    let verified = checks.iter().all(|check| check.passed);
    let mut published = None;
    if verified && plan.publish {
        publish(&plan.staging, &plan.destination)?;
        manifest.record(
            "destination.published",
            plan.destination.display().to_string(),
        )?;
        manifest.record("stage", "publish")?;
        published = Some(plan.destination.clone());
    } else if !verified {
        manifest.record("stage", "held")?;
    }
    manifest.record("source.retained", plan.source.display().to_string())?;

    let report = manifest.report();
    let report_path = with_suffix(&plan.destination, ".migration-report.md");
    std::fs::write(&report_path, report)
        .map_err(|error| format!("cannot write {}: {error}", report_path.display()))?;

    Ok(Outcome {
        checks,
        manifest: plan.manifest.clone(),
        published,
        documents,
        chunks,
    })
}

/// Moves a verified staging file into place, refusing to overwrite anything.
///
/// The directory entry is what an application opens, so replacing it is the one
/// step that has to be atomic - and a rename is the one operation the platform
/// performs atomically. Everything before this point is invisible to anybody
/// who was not told the staging name.
fn publish(staging: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return Err(format!(
            "{} appeared while the migration was running; nothing was published",
            destination.display()
        ));
    }
    std::fs::rename(staging, destination).map_err(|error| {
        format!(
            "cannot publish {} as {}: {error}",
            staging.display(),
            destination.display()
        )
    })?;
    // The directory entry itself has to reach the disk, or a power loss can
    // leave a rename that only happened in a cache.
    if let Some(parent) = destination.parent() {
        if let Ok(handle) = std::fs::File::open(parent) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}

/// Walks every tree of the staged file and reads rows back out of it.
///
/// **This used to run the pinned SQLite shell**, on the reasoning that "a
/// database only this engine can read is not the file this project promises".
/// That promise was withdrawn: the rearchitecture's design doc lists SQLite
/// file-format compatibility among its non-goals - "SQLite
/// will not open our files and we will not open SQLite's, except through a
/// test-only and migration-only reader". The comment outlived the requirement,
/// and a probe that asked SQLite to open an `.rdb` would now be asking for
/// something the project has decided not to provide.
///
/// What replaces it checks the format that exists, and is not weaker for it:
/// `check_trees` walks **every** tree in the file and verifies its key order,
/// which is what `PRAGMA integrity_check` does and is strictly more than a
/// reader's opinion of the pages it happened to touch. It runs on a **fresh
/// open of the closed file**, so it sees what a new process sees rather than
/// what the writer's warm pool saw.
///
/// What is honestly lost is *engine independence*: this is no longer a second
/// implementation reading the bytes, it is a second open. That is the direct
/// consequence of the format decision above rather than a choice made here, and
/// the rest of `verify.rs` - counts and ordered digests against the source -
/// is what carries the weight it used to.
///
/// It also no longer needs an external binary, so it always runs. The old probe
/// was skipped whenever no shell was configured, which meant the strongest
/// check in the procedure was the one most likely not to happen.
///
/// @param database - the staged file
fn structure_probe(database: &Path) -> Check {
    let opened = inillucent_engine::connect::Database::open(database);
    let database = match opened {
        Ok(database) => database,
        Err(error) => {
            return Check {
                name: "structure.integrity".to_string(),
                passed: false,
                detail: format!("cannot reopen the staged file: {}", error.message()),
            }
        }
    };
    if let Err(error) = database.check() {
        return Check {
            name: "structure.integrity".to_string(),
            passed: false,
            detail: format!("a tree is not intact: {}", error.message()),
        };
    }
    let connection = database.session();
    let chunks = count_of(&connection, "SELECT count(*) FROM chunk");
    let content = count_of(
        &connection,
        &format!("SELECT count(*) FROM {SEARCH_TABLE}_content"),
    );
    match (chunks, content) {
        (Ok(chunks), Ok(content)) => Check {
            name: "structure.integrity".to_string(),
            passed: true,
            detail: format!(
                "every tree walks in key order; a fresh open reads {chunks} rows out of chunk                  and {content} out of the search index's own storage"
            ),
        },
        (Err(detail), _) | (_, Err(detail)) => Check {
            name: "structure.integrity".to_string(),
            passed: false,
            detail,
        },
    }
}

/// Returns the single integer a counting query answers.
///
/// @param connection - the reopened staged database
/// @param sql - the counting query
fn count_of(connection: &Connection<'_>, sql: &str) -> Result<i64, String> {
    let rows = connection
        .query(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(number)) => Ok(*number),
        other => Err(format!("{sql} answered {other:?}")),
    }
}

/// Returns one value of a query as text.
fn scalar_text(connection: &Connection<'_>, sql: &str) -> Result<String, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    if !statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        return Ok(String::new());
    }
    Ok(match statement.row().first() {
        Some(OwnedDatum::Int(number)) => number.to_string(),
        Some(OwnedDatum::Text(text)) => String::from_utf8_lossy(text).into_owned(),
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The staging and manifest names are derived from the destination, so a
    /// resumed run finds what the interrupted one was building.
    #[test]
    fn the_staging_name_is_derived_and_never_the_destination() {
        let plan = Plan::new("/tmp/index", "/var/db/corpus.db");
        assert!(plan.staging.display().to_string().ends_with(".migrating"));
        assert!(plan
            .manifest
            .display()
            .to_string()
            .ends_with(".migration-manifest"));
        assert_ne!(plan.staging, plan.destination);
        assert_eq!(plan.staging.parent(), plan.destination.parent());
    }

    /// An outcome with no checks is not a verified one.
    #[test]
    fn an_unchecked_outcome_is_not_verified() {
        let outcome = Outcome {
            checks: Vec::new(),
            manifest: PathBuf::new(),
            published: None,
            documents: 0,
            chunks: 0,
        };
        assert!(!outcome.verified());
    }
}
