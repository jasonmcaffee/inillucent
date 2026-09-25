//! `VACUUM` on a connection the caller opened over its own file system.
//!
//! Invariant: **a statement never moves the connection onto a different file
//! system than the one it was opened on.**
//!
//! The defect this file exists for (task-1946, H2): `vacuum_in_place` rebuilt
//! into a scratch file and then did `*self = ImportedDatabase::open(...)` twice.
//! `ImportedDatabase::open` constructs `Arc::new(OsVfs::new())`, so the
//! connection came back on the operating system's file system whatever it went
//! in on - and `crates/inillucent-engine/src/rebuild.rs` compounded it by
//! calling `std::fs::rename`, `std::fs::read_dir` and `std::fs::remove_file`
//! directly.
//!
//! An application on `MemoryVfs`, `SimVfs`, or its own encrypting VFS therefore
//! got one of two things from `VACUUM` or `PRAGMA incremental_vacuum`: an
//! `Open: The system cannot find the path specified`, because nothing on the
//! real disk answered to the path string; or, if a real file happened to exist
//! at that string, a silent continuation on the operating system's file system
//! for the rest of the session. The second is the one worth a test: nothing
//! fails, and every later read and write goes to the wrong place.
//!
//! The same reopen happens in `PRAGMA incremental_vacuum`, so both are here.

use std::sync::Arc;

use inillucent_engine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::{AccessMode, DbPath, MemoryVfs, Vfs};

/// The page size and pool size these cases run at.
const PAGE_SIZE: usize = 4_096;

/// How many frames the pool holds.
const FRAMES: usize = 64;

/// The path the database takes inside the in-memory file system.
///
/// **Deliberately a path that also names a real directory that exists.** If it
/// were a name nothing on disk could answer to, the old code would have failed
/// loudly at the reopen and the interesting half of this defect - a `VACUUM`
/// that succeeds against the wrong file system - could not be reached. The file
/// itself is never created on disk by these tests, and the assertions check that.
///
/// @param tag - what to name this case's database after
fn memory_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "inillucent-vacuum-on-vfs-{}-{tag}.rdb",
        std::process::id()
    ))
}

/// Runs a statement of any kind, failing the test with the statement in the
/// message.
///
/// @param engine - the open connection
/// @param sql - the statement
fn run(engine: &mut ImportedDatabase, sql: &str) -> Vec<Vec<OwnedDatum>> {
    engine
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|why| panic!("{sql}: {}", why.message()))
        .rows
}

/// Returns the single integer one query answers with.
///
/// @param engine - the open connection
/// @param sql - a query returning one row of one integer
fn count(engine: &mut ImportedDatabase, sql: &str) -> i64 {
    match run(engine, sql).first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("{sql} answered {other:?}"),
    }
}

/// Builds a database with enough free space that `VACUUM` has work to do.
///
/// @param engine - the open connection
fn build(engine: &mut ImportedDatabase) {
    run(engine, "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)");
    run(engine, "BEGIN");
    for row in 0..2000 {
        run(
            engine,
            &format!("INSERT INTO t VALUES ({row}, '{}')", "x".repeat(200)),
        );
    }
    run(engine, "COMMIT");
    run(engine, "DELETE FROM t WHERE id > 10");
}

/// `VACUUM` on a `MemoryVfs` connection stays on that `MemoryVfs`, leaves no
/// file on disk, and still answers.
#[test]
fn vacuum_stays_on_the_file_system_the_connection_was_opened_on() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = memory_path("vacuum");
    let db_path = DbPath::new(path.to_string_lossy().as_ref());
    let _ = std::fs::remove_file(&path);

    let mut engine = ImportedDatabase::create_on(Arc::clone(&vfs), path.clone(), PAGE_SIZE, FRAMES)
        .expect("the database is created in memory");
    build(&mut engine);
    let before = count(&mut engine, "SELECT count(*) FROM t");
    assert_eq!(before, 11, "the fixture is not what the test assumes");

    run(&mut engine, "VACUUM");

    // The rows are still there, read through the connection that ran it.
    assert_eq!(
        count(&mut engine, "SELECT count(*) FROM t"),
        before,
        "VACUUM lost rows"
    );
    assert_eq!(
        count(&mut engine, "SELECT count(*) FROM t WHERE id = 1"),
        1,
        "VACUUM lost a specific row"
    );

    // And the file system it is on is still the one it was opened on: the
    // in-memory directory holds the database, and the real disk does not.
    assert!(
        vfs.access(&db_path, AccessMode::Exists)
            .expect("the in-memory file system answers"),
        "the database is no longer in the file system the connection was opened on"
    );
    assert!(
        !path.exists(),
        "VACUUM wrote {} onto the real disk, so the connection moved to OsVfs",
        path.display()
    );

    // Nothing the rebuild leaves behind is on the real disk either - not the
    // scratch file, and not a log segment.
    let strays: Vec<String> = std::fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| {
            name.starts_with(&format!(
                "inillucent-vacuum-on-vfs-{}-vacuum",
                std::process::id()
            ))
        })
        .collect();
    assert!(
        strays.is_empty(),
        "VACUUM left these on the real disk: {strays:?}"
    );

    drop(engine);
}

/// `PRAGMA incremental_vacuum` reaches the same reopen and has the same rule.
///
/// **The argument is not optional here.** `reclaim_free_pages` returns without
/// doing anything when the free list is shorter than the number asked for, and
/// a bare `PRAGMA incremental_vacuum` asks for more pages than this fixture ever
/// frees - so the version of this test without the `(1)` reached no rebuild, no
/// rename and no reopen, and passed with the whole fix reverted.
#[test]
fn incremental_vacuum_stays_on_the_file_system_too() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = memory_path("incremental");
    let db_path = DbPath::new(path.to_string_lossy().as_ref());
    let _ = std::fs::remove_file(&path);

    let mut engine = ImportedDatabase::create_on(Arc::clone(&vfs), path.clone(), PAGE_SIZE, FRAMES)
        .expect("the database is created in memory");
    // `incremental_vacuum` is silent unless `auto_vacuum` is `incremental`,
    // which is the reference's behaviour and this engine's.
    run(&mut engine, "PRAGMA auto_vacuum = incremental");
    build(&mut engine);
    let before = count(&mut engine, "SELECT count(*) FROM t");
    let pages_before = count(&mut engine, "PRAGMA page_count");

    run(&mut engine, "PRAGMA incremental_vacuum(1)");

    assert!(
        count(&mut engine, "PRAGMA page_count") < pages_before,
        "the pragma reclaimed nothing, so it never reached the rebuild this test is about:          still {pages_before} pages"
    );

    assert_eq!(
        count(&mut engine, "SELECT count(*) FROM t"),
        before,
        "incremental_vacuum lost rows"
    );
    assert!(
        vfs.access(&db_path, AccessMode::Exists)
            .expect("the in-memory file system answers"),
        "the database is no longer in the file system the connection was opened on"
    );
    assert!(
        !path.exists(),
        "incremental_vacuum wrote {} onto the real disk",
        path.display()
    );

    drop(engine);
}

/// A `VACUUM` on a real file still works, which is the half a VFS change could
/// break without any of the assertions above noticing.
#[test]
fn vacuum_on_the_real_file_system_still_works() {
    let path = std::env::temp_dir().join(format!(
        "inillucent-vacuum-on-vfs-real-{}.rdb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("the database is created");
    build(&mut engine);
    let before = count(&mut engine, "SELECT count(*) FROM t");

    run(&mut engine, "VACUUM");
    assert_eq!(count(&mut engine, "SELECT count(*) FROM t"), before);
    assert!(path.exists(), "the database is not on the disk any more");

    drop(engine);

    // Reopening proves the rename landed and the stale segments went, which is
    // what `remove_log_segments` walking by name rather than by listing has to
    // keep doing.
    let mut reopened = ImportedDatabase::open(path.clone(), PAGE_SIZE, FRAMES)
        .expect("the vacuumed database reopens");
    assert_eq!(count(&mut reopened, "SELECT count(*) FROM t"), before);
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}
