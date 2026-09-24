//! Nikaya's startup migration, with the process killed inside it.
//!
//! Invariant: **a real process killed between the `ALTER TABLE ADD COLUMN` and
//! the `CREATE INDEX` that follows it leaves a database the next startup can
//! open, read every column of, and finish migrating.** The kill is the
//! operating system ending a process - `TerminateProcess` on Windows, `SIGKILL`
//! on Unix - so nothing runs a destructor, flushes a buffer or closes a file.
//!
//! ## Why this half is here and the other half is not
//!
//! `crates/inillucent/tests/story_nikaya.rs` runs the same sequence in process
//! at every arm and needs nothing but the engine. This one spawns the shipped
//! shell, so it declares `requires = ["programs"]` and prints `; skipping` on a
//! machine that has not built one. Keeping them in one target would have made
//! the whole story skippable, and a story that skips is a story nobody knows
//! did not run.
//!
//! ## What the file is allowed to look like afterwards
//!
//! Either state is correct and the test says which it found. The column is
//! added by a statement, and a statement either committed before the kill or it
//! did not:
//!
//! - the column is there, and the index is not, because the kill landed between
//!   them - which is the state this is named for;
//! - the column is not there either, because the `ALTER` had not reached the
//!   file when the process ended.
//!
//! What is **not** allowed is a third state: the neighbours unreadable, the
//! file unopenable, or a schema that says the column is there when reading it
//! fails. `93c7261` is the first of those and `bd16a3e` is the second.
//!
//! ## The page size reaches the file and not the shell
//!
//! The file is created in process at the arm's geometry and the shell is then
//! handed the path. The shipped programs have no way to ask for a page size -
//! `inillucent create` takes a path and nothing else - so every database any
//! user of the command line has ever made is 32,768 bytes a page, which is the
//! same gap task-2033 lives in. The pool reads the page size out of the meta
//! record, so the shell writes the file correctly; what it reports through
//! `PRAGMA page_size` is the constant.

use std::path::{Path, PathBuf};

use inillucent_compat::cliproc::{program, write_and_crash};
use inillucent_compat::matrix::{Arm, Scale};
use inillucent_compat::nikaya::{every_column, seed, LEDGER};
use inillucent_compat::scenario;
use inillucent_compat::stories::{ask, open, reopen_and_check, run};

/// Where the shell's databases go.
///
/// Under `_agent_output/`, which is gitignored, and named for the arm so two
/// arms never share a file - the same rule `Arm::area` follows for the in
/// process stories.
///
/// @param arm - the configuration this run is at
/// @param tag - which case it is
fn area(arm: &Arm, tag: &str) -> PathBuf {
    let path = inillucent_compat::workspace_root()
        .join("_agent_output/story-nikaya-crash")
        .join(format!("{}-{tag}", arm.name));
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// A kill between the `ALTER TABLE` and the `CREATE INDEX` leaves a database
/// the next startup finishes.
fn a_kill_inside_the_migration_leaves_it_finishable(arm: &Arm, _unused: &Path) {
    let shell = program("inillucent-shell");
    let directory = area(arm, "mid-migration");
    let path = directory.join("nikaya.rdb");
    let documents = Scale::from_env().pick(24, 400);

    // The corpus, and the part of the migration that has already run: a real
    // startup creates the ledger, reads it, and then applies what is missing.
    let before;
    {
        let database = open(arm, &path);
        let connection = database.session();
        seed(&connection, documents);
        run(&connection, LEDGER);
        before = every_column(&connection);
        assert!(
            before.lines().count() > documents * 3,
            "the corpus read back as {} lines for {documents} documents, so the seed wrote \
             nothing",
            before.lines().count()
        );
    }

    // The shell runs the `ALTER` and is killed with the `CREATE INDEX` still to
    // come. `write_and_crash` waits for the statements to have run and then
    // ends the process, so the cut is after the `ALTER` and before anything
    // else - which is the point the migration is most exposed at.
    let said = write_and_crash(
        &shell,
        &path,
        "ALTER TABLE chunk ADD COLUMN embedded_at INTEGER;",
    );
    assert!(
        said.contains("written"),
        "the shell was killed before it ran the ALTER, so this case cut somewhere else:\n{said}"
    );

    // The next startup opens it.
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        every_column(&connection),
        before,
        "the killed migration changed a table it did not name, at the {} arm",
        arm.name
    );

    let added = ask(
        &connection,
        "SELECT count(*) FROM pragma_table_info('chunk') WHERE name = 'embedded_at'",
    );
    assert!(
        added == "0" || added == "1",
        "`embedded_at` is present {added} times after the kill, at the {} arm",
        arm.name
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM sqlite_master WHERE name = 'chunk_embedded_at_idx'"
        ),
        "0",
        "the index exists although the process was killed before the statement that makes it, \
         at the {} arm",
        arm.name
    );

    // And the next startup finishes the migration, which is the whole of what a
    // crash inside one has to leave possible. The `ALTER` is run only when the
    // column is not there, because that is what a startup does: it asks.
    if added == "0" {
        run(
            &connection,
            "ALTER TABLE chunk ADD COLUMN embedded_at INTEGER",
        );
    }
    run(
        &connection,
        "CREATE INDEX chunk_embedded_at_idx ON chunk (embedded_at)",
    );
    run(
        &connection,
        "INSERT INTO schema_migration (name, applied_at) VALUES ('003_embedded_flag', 1700000000)",
    );
    run(
        &connection,
        "UPDATE chunk SET embedded_at = 1700000500 WHERE ordinal = 0",
    );

    drop(connection);
    drop(database);
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        every_column(&connection),
        before,
        "finishing the migration changed the corpus, at the {} arm",
        arm.name
    );
    // Rule 1.6: the index built after the crash agrees with the table it was
    // built over.
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM chunk WHERE embedded_at = 1700000500"
        ),
        ask(&connection, "SELECT count(*) FROM chunk WHERE ordinal = 0"),
        "the index built after the killed migration disagrees with its table, at the {} arm",
        arm.name
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT name FROM schema_migration ORDER BY name"
        ),
        "003_embedded_flag",
        "the ledger does not record the migration the restart finished, at the {} arm",
        arm.name
    );
}

scenario!(
    a_kill_inside_the_migration_leaves_it_finishable,
    a_kill_inside_the_migration_leaves_it_finishable
);

/// A kill during `ANALYZE` on a migrated database leaves it openable.
///
/// **`bd16a3e`, in the shape it actually happened in.** `ANALYZE` on a migrated
/// 6.9 GB corpus wrote a page stamped by an abandoned log stream, and the file
/// could never be opened again. The in process story runs `ANALYZE` and
/// reopens; this one ends the process while `ANALYZE` is the last thing it ran,
/// so the log stream is abandoned rather than closed, which is the condition
/// the page was stamped under.
fn a_kill_after_analyze_leaves_the_file_openable(arm: &Arm, _unused: &Path) {
    let shell = program("inillucent-shell");
    let directory = area(arm, "after-analyze");
    let path = directory.join("analyzed.rdb");
    let documents = Scale::from_env().pick(24, 400);

    let before;
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
        before = every_column(&connection);
    }

    let said = write_and_crash(&shell, &path, "ANALYZE;");
    assert!(
        said.contains("written"),
        "the shell was killed before it ran ANALYZE:\n{said}"
    );

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        every_column(&connection),
        before,
        "the corpus reads differently after a kill that followed ANALYZE, at the {} arm",
        arm.name
    );
    // The file is still writable, which is the half a reopen alone does not
    // ask: `bd16a3e`'s database opened for exactly as long as it took to fail.
    run(
        &connection,
        "UPDATE chunk SET embedded_at = 1700000600 WHERE ordinal = 1",
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM chunk WHERE embedded_at = 1700000600"
        ),
        documents.to_string(),
        "the file could be opened after the kill and not written to, at the {} arm",
        arm.name
    );
}

scenario!(
    a_kill_after_analyze_leaves_the_file_openable,
    a_kill_after_analyze_leaves_the_file_openable
);
