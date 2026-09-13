//! Backup: what inillucent makes of the file it produces.
//!
//! Invariant: a backup reopens through `inillucent_engine::connect::Database`
//! and reads back what was written before it.
//!
//! **Re-pointed off the pinned SQLite shell.** This file's invariant used to
//! read "a backup only this engine can open is not a backup, so the result is
//! handed to the pinned build and read back through it" - true of the old
//! engine (`inillucent-session`), which read and wrote the actual SQLite file
//! format. The shipping engine does not: its magic bytes are `RDB2`, not
//! `SQLite format 3\0`, `docs/feature-comparison.md`'s "file format" row says
//! so, and this file said the reverse of it below before this note replaced
//! it - "a SQLite file cannot be opened ... it is imported". Handed *any*
//! file this engine writes, backed up or not, the pinned shell answers
//! `Parse error in 2nd command line argument: file is not a database (26)`;
//! that is not a defect a backup could fix; it is confirmation the file is
//! genuinely `inillucent`'s own format, not SQLite's. What "a backup only
//! this engine can open is not a backup" now means is: open the copy through
//! `Database::open`, run the engine's own `PRAGMA integrity_check` on it, and
//! read a row back - the one reader the shipping engine's file format
//! actually promises to work with.
//!
//! **Incremental blob access and serialize/deserialize are gone from this
//! file, not ported.** `docs/invariants/layering.toml`'s note on the `inillucent`
//! facade names backup, blobs and serialize together as the features the new
//! engine deliberately does not have, and `inillucent_engine`'s source confirms
//! it: there is no `Blob` type, no `serialize`/`deserialize` function, and
//! `ImportedDatabase::backup_into` (the old engine's incremental, page-by-page
//! backup) exists on the new engine only as a private helper behind
//! `Database::backup_to`, which checkpoints the log into the file and then
//! copies the whole thing rather than stepping page by page - "this engine is
//! single threaded and one file is one pool: there is no second writer to
//! race", in its own doc comment. That is enough to keep the one test below
//! that asks the same question a backup answers - does the engine read what
//! comes out - by asking it a different way; it is not enough for a blob
//! opened by rowid or a database serialised to an in-memory buffer, which
//! have no route into the new engine at all. The seven deleted cases were: a
//! blob reads ranges, a blob writes ranges in place, a read-only blob refuses
//! a write, a blob on a deleted row is refused, a database serialises to
//! bytes SQLite can read, bytes SQLite wrote deserialise, and a deserialised
//! database can be written and serialised again.

use std::path::PathBuf;

use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// Returns a fresh scratch path.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/services");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Returns a database's bytes with its version markers blanked.
///
/// Everything a page holds is compared, and the two numbers that describe the
/// *file's* history rather than its contents are not: the change counter and
/// the version it was last valid for. A copy has to differ in those - they are
/// how another connection learns its cache is stale, and a copy whose counter
/// matched the original's would be a file readers believed they had already
/// seen.
fn pages(bytes: &[u8]) -> Vec<u8> {
    let mut copy = bytes.to_vec();
    for range in [24..28usize, 92..96] {
        if let Some(window) = copy.get_mut(range) {
            window.fill(0);
        }
    }
    copy
}

/// A backup reproduces the file, and the engine reads what comes out.
///
/// Page for page rather than rebuilt: a backup is not a `VACUUM`, so the two
/// files are compared byte for byte rather than row for row. The copy is then
/// read back through a second, independent `Database::open` - not the
/// connection that wrote it - the same way §1.4 of the testing standard reads
/// back anything claiming to be durable.
#[test]
fn a_backup_reproduces_the_database() {
    let source_path = scratch("backup-source");
    let destination_path = scratch("backup-destination");
    {
        let source = Database::open(&source_path).expect("the source opens");
        let connection = source.connect();
        connection
            .execute_batch(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 CREATE INDEX t_b ON t(b);
                 INSERT INTO t VALUES (1,'one'),(2,'two'),(3,'three');",
            )
            .expect("the rows are written");
        source
            .backup_to(&destination_path)
            .expect("the backup runs");
    }
    let source_bytes = std::fs::read(&source_path).expect("the source is readable");
    let copy_bytes = std::fs::read(&destination_path).expect("the copy is readable");
    assert_eq!(
        pages(&source_bytes),
        pages(&copy_bytes),
        "the backup is not a copy of the file"
    );

    let backup = Database::open(&destination_path).expect("the backup opens through inillucent");
    let connection = backup.connect();
    let check = connection
        .query("PRAGMA integrity_check")
        .expect("integrity_check runs")
        .first()
        .and_then(|row| row.first())
        .cloned();
    assert_eq!(
        check,
        Some(OwnedDatum::Text(b"ok".to_vec())),
        "the backup's own integrity_check did not say ok: {check:?}"
    );
    let rows = connection
        .query("SELECT b FROM t ORDER BY a")
        .expect("the copy reads back");
    assert_eq!(
        rows.first().and_then(|row| row.first()).cloned(),
        Some(OwnedDatum::Text(b"one".to_vec())),
        "the backup did not read back the first row"
    );
}
