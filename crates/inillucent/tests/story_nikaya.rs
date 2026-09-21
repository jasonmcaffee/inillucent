//! Nikaya's startup, against populated tables, at every arm.
//!
//! Invariant: **a schema migration that runs beside populated tables leaves
//! every column of every one of them readable, and says so by reading them all
//! back after a reopen.**
//!
//! ## The escape this is shaped around
//!
//! `93c7261`. Nikaya runs a migration at every startup: create the ledger if it
//! is not there, ask it what has run, run what has not, record it. On a
//! populated database one of those `CREATE TABLE`s corrupted a table beside it,
//! and **`count(*)` still answered correctly** - so a story that checked the
//! table was still there would have passed. What found it was reading the rows.
//!
//! Two more escapes are in the same sequence. `bd16a3e` is `ANALYZE` on a
//! migrated database writing a page stamped by an abandoned log stream, after
//! which the file could never be opened again; this story runs `ANALYZE`,
//! closes, and opens. And `003_embedded_flag.sql` is an `ALTER TABLE ADD
//! COLUMN` followed by a `CREATE INDEX` on the column it added, which is the
//! shape rule 1.6 exists for: the index is checked by asking the same question
//! through the table and through the index.
//!
//! ## Why the statements are Nikaya's own
//!
//! They are copied from `C:/jason/dev/nikaya/server/src/db.rs` and
//! `migrations/inillucent/*.sql` rather than written to look like them. The
//! whole point of a consumer story is that it is the consumer's program: a
//! sequence invented here would be a sequence nobody runs, and the escape it is
//! shaped around was in the order the real one goes in.
//!
//! None of Nikaya's data is here. The rows are generated, and the text is sized
//! so that values cross the extent threshold at both page sizes the matrix
//! runs - which is the other half of why this is a story rather than a unit
//! test of `ALTER TABLE`.

use std::path::Path;

use inillucent_compat::matrix::{Arm, Scale};
use inillucent_compat::nikaya::{every_column, seed};
use inillucent_compat::scenario;
use inillucent_compat::stories::{ask, open, reopen_and_check, run, the_same_two_ways};
use inillucent_engine::connect::Database;

/// Reads the stories that are known to fail, and the ticket that owns each.
///
/// The same file shape `story_rag.rs` and `story_edges.rs` read: one line per
/// failing story and arm, a tab, and the ticket. **An entry that no longer
/// describes a failure is itself a failure**, so when the ticket lands the
/// story goes red until the line is removed.
fn allow_listed(key: &str) -> Option<String> {
    let path = inillucent_compat::workspace_root().join("tests/workloads/nikaya/story.allow.list");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some((named, said)) = line.split_once('\t') {
            if named.trim() == key {
                return Some(said.trim().to_string());
            }
        }
    }
    None
}

/// Opens the migrated database again and checks it, or records a known failure.
///
/// **The check is the story's own rather than the helper's, so an arm this
/// story finds broken can be written off by ticket while that ticket is open.**
/// `reopen_and_check` panics, which is right everywhere else it is used; here
/// the failure is looked up in `tests/workloads/nikaya/story.allow.list` first,
/// and an entry that no longer describes a failure fails the story instead. The
/// list is empty: task-2055 was the one entry it has had, and the fix took the
/// line with it.
///
/// Returns `None` when the arm is written off, which ends the story there.
///
/// @param arm - the arm being run
/// @param path - the database
/// @param story - the story's name, for the allow list key
fn reopen_and_read(arm: &Arm, path: &Path, story: &str) -> Option<Database> {
    let database = open(arm, path);
    let key = format!("{story}::{}", arm.test_name());
    match (database.check(), allow_listed(&key)) {
        (Ok(()), None) => Some(database),
        (Ok(()), Some(said)) => panic!(
            "`{key}` is in tests/workloads/nikaya/story.allow.list against `{said}` and the \
             database was sound after the reopen. Delete the line: a fixed defect left listed \
             reads as coverage and is not."
        ),
        (Err(why), Some(said)) => {
            println!(
                "{key}: the reopened database is not sound: {}; allow listed against {said}",
                why.message()
            );
            None
        }
        (Err(why), None) => panic!(
            "{} is not sound after a reopen at the {} arm: {}. If this is a defect another \
             ticket owns, add `{key}` to tests/workloads/nikaya/story.allow.list with the \
             ticket that owns it.",
            path.display(),
            arm.name,
            why.message()
        ),
    }
}

/// Nikaya's startup migration, run against a populated database.
///
/// The sequence is `db.rs::migrate` and `003_embedded_flag.sql`, in order:
/// create the ledger, ask it what has run, create a table, add a column to a
/// populated one, index the column, record the migration. Then read every
/// column of every table back and compare it to what was there before.
fn a_startup_migration_leaves_every_neighbour_readable(arm: &Arm, area: &Path) {
    let path = area.join("nikaya.rdb");
    let documents = Scale::from_env().pick(64, 2_000);
    let before;
    {
        let database = open(arm, &path);
        let connection = database.session();
        seed(&connection, documents);
        before = every_column(&connection);
        assert!(
            before.lines().count() > documents * 3,
            "the corpus read back as {} lines for {documents} documents, so the seed wrote \
             nothing and everything below would be asserting about an empty database",
            before.lines().count()
        );

        // `db.rs::migrate`, statement for statement.
        run(
            &connection,
            "CREATE TABLE IF NOT EXISTS schema_migration (\
               name       TEXT PRIMARY KEY,\
               applied_at INTEGER NOT NULL\
             )",
        );
        assert_eq!(
            ask(
                &connection,
                "SELECT name FROM schema_migration WHERE name = '003_embedded_flag'"
            ),
            "",
            "the ledger says a migration has run on a database that has never seen one"
        );
        // `004_embedding_queue.sql`: a new table beside the populated ones.
        run(
            &connection,
            "CREATE TABLE IF NOT EXISTS embedding_queue (\
               chunk_id  TEXT PRIMARY KEY REFERENCES chunk(id) ON DELETE CASCADE,\
               queued_at INTEGER NOT NULL\
             )",
        );
        // `003_embedded_flag.sql`: a column added to a populated table, then an
        // index on the column that was just added.
        run(
            &connection,
            "ALTER TABLE chunk ADD COLUMN embedded_at INTEGER",
        );
        run(
            &connection,
            "CREATE INDEX IF NOT EXISTS chunk_embedded_at_idx ON chunk (embedded_at)",
        );
        run(
            &connection,
            "INSERT INTO schema_migration (name, applied_at) VALUES ('003_embedded_flag', 1700000000)",
        );

        // Every column of every neighbouring table still reads the same.
        let after = every_column(&connection);
        assert_eq!(
            after, before,
            "the migration changed a table it did not name, at the {} arm",
            arm.name
        );
        assert_eq!(
            ask(
                &connection,
                "SELECT name FROM schema_migration ORDER BY name"
            ),
            "003_embedded_flag"
        );
    }

    // Reopen, and read all of it again from a handle that did not write it.
    let database = reopen_and_read(
        arm,
        &path,
        "a_startup_migration_leaves_every_neighbour_readable",
    );
    let Some(database) = database else {
        return;
    };
    let connection = database.session();
    assert_eq!(
        every_column(&connection),
        before,
        "the migration's neighbours read differently after a reopen, at the {} arm",
        arm.name
    );
    assert_eq!(
        ask(&connection, "SELECT count(*) FROM embedding_queue"),
        "0",
        "the table the migration added is not there after a reopen"
    );

    // The added column is writable, and the index over it agrees with the
    // table - rule 1.6, and the shape `003_embedded_flag.sql` is.
    run(
        &connection,
        "UPDATE chunk SET embedded_at = 1700000500 WHERE ordinal = 0",
    );
    let marked = the_same_two_ways(
        &connection,
        "SELECT count(*) FROM chunk WHERE ordinal = 0",
        "SELECT count(*) FROM chunk WHERE embedded_at = 1700000500",
        "the index on the column ALTER TABLE added",
    );
    assert_eq!(
        marked,
        documents.to_string(),
        "one chunk per document should have been marked, at the {} arm",
        arm.name
    );
}

scenario!(
    a_startup_migration_leaves_every_neighbour_readable,
    a_startup_migration_leaves_every_neighbour_readable
);

/// `ANALYZE` after a migration, then close, then open.
///
/// **`bd16a3e`.** `ANALYZE` on a migrated 6.9 GB corpus wrote a page stamped by
/// an abandoned log stream, and the file could never be opened again - so the
/// failure was not in `ANALYZE` at all, it was in the next open. Nothing about
/// it is visible without closing the database, which is why this is a story and
/// not a statement test.
///
/// The three maintenance commands are run in the order a real startup would
/// reach them, each followed by a reopen and a full read back, so a file that
/// stops opening names the command that made it stop.
fn maintenance_after_a_migration_leaves_the_file_openable(arm: &Arm, area: &Path) {
    let path = area.join("maintained.rdb");
    let documents = Scale::from_env().pick(24, 1_200);
    let expected;
    {
        let database = open(arm, &path);
        let connection = database.session();
        seed(&connection, documents);
        run(
            &connection,
            "ALTER TABLE chunk ADD COLUMN embedded_at INTEGER",
        );
        run(
            &connection,
            "CREATE INDEX chunk_embedded_at_idx ON chunk (embedded_at)",
        );
        expected = every_column(&connection);
    }

    for command in ["ANALYZE", "REINDEX", "VACUUM", "PRAGMA wal_checkpoint"] {
        {
            let database = open(arm, &path);
            let connection = database.session();
            run(&connection, command);
        }
        let database = reopen_and_check(arm, &path);
        let connection = database.session();
        assert_eq!(
            every_column(&connection),
            expected,
            "`{command}` changed what the corpus reads as, at the {} arm",
            arm.name
        );
        assert_eq!(
            ask(
                &connection,
                "SELECT count(*) FROM chunk WHERE embedded_at IS NULL"
            ),
            (documents * 2).to_string(),
            "`{command}` lost the column the migration added, at the {} arm",
            arm.name
        );
    }
}

scenario!(
    maintenance_after_a_migration_leaves_the_file_openable,
    maintenance_after_a_migration_leaves_the_file_openable
);

/// A migration that fails part way leaves the database as it found it.
///
/// Nikaya runs each migration inside a transaction and records it in the same
/// one, so a migration that fails must leave neither its effect nor its ledger
/// row. The statement that fails here is the one that would fail in production:
/// a second `ALTER TABLE ADD COLUMN` naming a column that is already there.
fn a_migration_that_fails_leaves_no_half_of_itself(arm: &Arm, area: &Path) {
    let path = area.join("failed.rdb");
    let documents = Scale::from_env().pick(32, 400);
    let before;
    {
        let database = open(arm, &path);
        let connection = database.session();
        seed(&connection, documents);
        run(
            &connection,
            "CREATE TABLE IF NOT EXISTS schema_migration (\
               name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)",
        );
        run(
            &connection,
            "ALTER TABLE chunk ADD COLUMN embedded_at INTEGER",
        );
        before = every_column(&connection);

        run(&connection, "BEGIN");
        run(
            &connection,
            "CREATE TABLE IF NOT EXISTS embedding_queue (\
               chunk_id TEXT PRIMARY KEY, queued_at INTEGER NOT NULL)",
        );
        let refused = connection
            .execute("ALTER TABLE chunk ADD COLUMN embedded_at INTEGER")
            .expect_err("a column that is already there is refused");
        assert_eq!(
            refused.code(),
            inillucent_base::PrimaryCode::Error,
            "the refusal was {} ({:?}), which is not the documented one",
            refused.message(),
            refused.code()
        );
        run(&connection, "ROLLBACK");

        assert_eq!(
            ask(&connection, "SELECT name FROM schema_migration"),
            "",
            "a migration that failed recorded itself anyway"
        );
    }

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        every_column(&connection),
        before,
        "the abandoned migration left something behind, at the {} arm",
        arm.name
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM sqlite_master WHERE name = 'embedding_queue'"
        ),
        "0",
        "the table created inside the abandoned transaction survived it"
    );
}

scenario!(
    a_migration_that_fails_leaves_no_half_of_itself,
    a_migration_that_fails_leaves_no_half_of_itself
);
