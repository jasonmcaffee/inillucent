//! `ANALYZE`, then reopened with the log unfolded.
//!
//! Invariant: **every tree the log names has a shape the replay can be told.**
//! The same one `autoindex_reopen.rs` carries. Recovery refuses a record naming a tree it was not given the shape of, which
//! is the right refusal - replaying into a guessed shape corrupts a file quietly - so the shapes have
//! to be derivable for *every* tree, not for most of them.
//!
//! **`ANALYZE` was the messenger for a defect in the page-LSN rule, not the cause.** The first six
//! tests here are the search that went looking for it in `ANALYZE` and did not find it; they are
//! kept because they pin what `ANALYZE` must keep doing. The last two are the reduction, and they
//! are about the rule.
//!
//! What the failure looks like, found against Nikaya's 6.9 GB mail database:
//!
//! ```text
//! > inillucent --db nikaya.rdb analyze
//! ok. sqlite_stat1 is up to date.
//! > inillucent --db nikaya.rdb query "SELECT count(*) FROM document"
//! Error [io]: could not open "nikaya.rdb": bad parameter or other API misuse:
//!            the log names tree 2147483712, which this recovery was not told the shape of
//! ```
//!
//! Isolated by reverting the variable rather than by matching a symptom: on the *same file*, an
//! ordinary `UPDATE` writes and reopens fine, and `ANALYZE` immediately afterwards leaves it
//! unopenable. Deterministic, about seven seconds, one command. `2147483712` is `0x8000_0040` - a
//! **provisional** root, `0x8000_0000` plus a counter, which is how a tree created inside the
//! transaction being replayed is named before it has a page. The catalog the shape derivation reads
//! holds real page numbers, so nothing there can place it.
//!
//! It matters more than a missing statistic: the failure is *silent at write time* - `ANALYZE`
//! prints `ok` - and appears only at the next open, which for a service is the next restart.
//!
//! What these three cover, and it is the class rather than the case: `ANALYZE` with an unfolded log
//! at one object, at sixty-odd objects (so the provisional counter reaches `0x8000_0040`, the exact
//! number the real failure reported), and twice over so the second run writes into a `sqlite_stat1`
//! the catalog already holds. All three pass, and they are kept because they pin what `ANALYZE`
//! itself must keep doing.
//!
//! ## What the 6.9 GB database had that a fresh one does not
//!
//! **A page carrying an LSN from a log stream that no longer exists**, and `ANALYZE` is the
//! messenger rather than the cause. Reproduced against a copy of the file task-1876 parked,
//! `nikaya.rdb.after-checkpoint-recovery`, which that ticket records the location of: copy it,
//! run `analyze`, reopen. Nine seconds, deterministic, and the same message.
//!
//! What the file says, read out of its own bytes:
//!
//! ```text
//! meta checkpoint_lsn 21,074,969,552      the position the log resumes at
//! page 3, the catalog leaf, is stamped 21,939,058,496
//! of the first 3,000 pages, 9 carry a stamp above the whole log's end (21,075,008,400)
//! ```
//!
//! Page 3 is stamped 864 million positions **beyond** the end of the log beside it. Recovery's
//! page-LSN rule - apply a record only when the page's stamp is below it - therefore skips every
//! record for that page, because the stamp says the page already has them. It does not: the stamp
//! is a position in an earlier stream that was abandoned when 24 segments were moved aside to
//! recover the file.
//!
//! So `ANALYZE` wrote `sqlite_stat1`'s catalog row into page 3, the row went to the log, the reopen
//! skipped that record, and the row was gone. A trace in `LearningRows` confirms it directly: the
//! applier is built from 62 checkpointed entries knowing trees `0x8000_0000..=0x8000_003D`, and
//! `learn` is **never called** for the catalog row that would have added `0x8000_003E`. The shape
//! derivation then has nothing to register the tree under and refuses - which is the message, and it
//! is the *second* thing that went wrong. The first is that a committed row was discarded silently.
//!
//! It is not specific to `ANALYZE` and not specific to the catalog. Any write to a page stamped
//! above the log's resumed position is discarded at the next open, and `PRAGMA integrity_check`
//! answers `ok` about the result, because the file really is structurally intact - it is missing a
//! row nothing can see was lost.
//!
//! ## The reduction
//!
//! There was no synthetic reduction at first, and the search that failed is worth keeping:
//! building and checkpointing, stealing pages with a 64-frame pool, truncating the log and
//! reopening does *not* reproduce it, because the page the write lands on reaches the file again
//! before the next open and the log is never asked. That search was after the *conditions* that set
//! the distance between the resumed position and the stale stamps, and it never found what sets it.
//!
//! The state is eight bytes: a page's LSN in bytes 0..8, with a checksum that agrees with it. That
//! is why a file in this state passes `PRAGMA integrity_check`, since the page is byte for byte
//! valid. (Format 1's checksum left the LSN out, so the eight bytes alone made the state; since
//! task-2074 the checksum covers them and `stamp_page` writes it again.) So the two tests at the
//! bottom of this file stamp the page directly and the variable disappears:
//! `a_page_stamped_above_the_logs_end_refuses_the_open` and
//! `a_log_below_the_files_high_water_resumes_above_it`. Both fail with the fix reverted,
//! the first printing the silent loss in as many words - a committed `CREATE TABLE` reading back as
//! `no such table: later`.
//!
//! The parked file is still the measurement of record, and it is preserved.
//!
//! The tests abandon the connection the way the crash tests do, so the log is still there to replay.
//! A tidy close checkpoints, which folds the log into the file and never reaches the code this is
//! about.

use std::path::{Path, PathBuf};

use inillucent_compat::facade::Database;
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns a scratch directory for one scenario.
///
/// @param name - what to name it after
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/analyze-reopen")
        .join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Runs statements and abandons the connection with its log unfolded.
///
/// @param path - the database file
/// @param sql - the statements to run
fn write_and_abandon(path: &Path, sql: &str) {
    // **A real crash, in a real process (task-1980).** What this has to leave
    // behind is a log nothing has folded into the file. The default is
    // `locking_mode = normal`, under which a connection checkpoints and
    // releases the file after every statement that wrote - so the statements
    // below would fold themselves down one at a time and there would be no
    // unfolded log to reopen.
    //
    // This used to ask for that with `PRAGMA locking_mode = EXCLUSIVE`, the
    // statements, and `PRAGMA locking_mode = NORMAL`, whose drop to `normal`
    // released the file without checkpointing. That release is gone: a
    // connection that let the file go with pages still dirty left the file
    // describing a database without the statement that had just succeeded, and
    // two writer processes lost 43% of their acknowledged commits to it. So the
    // crash is a real one now - the shell is killed while it waits for its next
    // line, and the operating system releases the locks, which is the thing
    // this was simulating all along.
    let shell = inillucent_compat::cliproc::program("inillucent-shell");
    let said = inillucent_compat::cliproc::write_and_crash(&shell, path, sql);
    assert!(
        said.contains("written"),
        "the statements did not run before the process was killed:\n{said}"
    );
}

/// Returns the rows a query answers over a freshly opened database.
///
/// @param path - the database file
/// @param sql - the query
fn read_back(path: &Path, sql: &str) -> Vec<Vec<Value<'static>>> {
    let database = Database::open(path).expect("the database reopens");
    let connection = database.session().expect("the connection opens");
    connection.query(sql).expect("the query runs")
}

/// The smallest form: one table, one index, `ANALYZE`, reopen.
#[test]
fn analyze_survives_a_reopen_with_the_log_unfolded() {
    let directory = scratch("one-table");
    let path = directory.join("stats.db");
    write_and_abandon(
        &path,
        "CREATE TABLE t(a TEXT, b INT);\n\
         CREATE INDEX t_a ON t(a);\n\
         INSERT INTO t VALUES ('x', 1), ('y', 2), ('z', 3);\n\
         ANALYZE;",
    );
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
}

/// The shape the real database had: enough objects that the provisional root is well past the first.
///
/// The counter that names a created tree starts at `0x8000_0000` and advances per object, so a
/// database with sixty-odd tables and indexes reaches `0x8000_0040` - which is the number the
/// original failure reported, and a test that only ever allocates the first provisional root would
/// pass while the bug stood.
#[test]
fn analyze_survives_a_reopen_on_a_schema_of_many_objects() {
    let directory = scratch("many-objects");
    let path = directory.join("stats.db");
    let mut sql = String::new();
    for table in 0..24 {
        sql.push_str(&format!(
            "CREATE TABLE t{table}(id TEXT PRIMARY KEY, a TEXT, b INT);\n"
        ));
        sql.push_str(&format!("CREATE INDEX t{table}_a ON t{table}(a);\n"));
        sql.push_str(&format!(
            "INSERT INTO t{table} VALUES ('x', 'p', 1), ('y', 'q', 2);\n"
        ));
    }
    sql.push_str("ANALYZE;");
    write_and_abandon(&path, &sql);
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
    // And the tables it measured still read, which is what says the replay applied rather than that
    // it merely did not refuse.
    let found = read_back(&path, "SELECT b FROM t23 WHERE id = 'y'");
    assert_eq!(found.len(), 1, "the corpus did not survive the replay");
}

/// `ANALYZE` twice, so the second run writes into a `sqlite_stat1` the catalog already holds.
#[test]
fn a_second_analyze_survives_a_reopen() {
    let directory = scratch("twice");
    let path = directory.join("stats.db");
    write_and_abandon(
        &path,
        "CREATE TABLE t(a TEXT, b INT);\n\
         CREATE INDEX t_a ON t(a);\n\
         INSERT INTO t VALUES ('x', 1), ('y', 2);\n\
         ANALYZE;",
    );
    let _ = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    write_and_abandon(&path, "INSERT INTO t VALUES ('z', 3);\nANALYZE;");
    let rows = read_back(&path, "SELECT count(*) FROM t");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(3)
    );
}

/// The real case's shape: the schema already checkpointed, and `ANALYZE` alone in the log.
///
/// This is what Nikaya's database was. Every table and index had real page roots written long
/// before, so the only tree `ANALYZE` creates is `sqlite_stat1`, and it is the only object in the
/// log whose root is provisional. The earlier tests all create the schema and analyse it in the
/// same breath, which means the catalog rows the shape derivation needs are in the same log - a
/// different situation, and the reason they pass.
#[test]
fn analyze_alone_in_the_log_survives_a_reopen() {
    let directory = scratch("checkpointed-schema");
    let path = directory.join("stats.db");
    // Built and closed tidily, so this half is folded into the file and out of the log.
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session().expect("the connection opens");
        let mut sql = String::new();
        for table in 0..24 {
            sql.push_str(&format!(
                "CREATE TABLE t{table}(id TEXT PRIMARY KEY, a TEXT, b INT);
"
            ));
            sql.push_str(&format!(
                "CREATE INDEX t{table}_a ON t{table}(a);
"
            ));
            sql.push_str(&format!(
                "INSERT INTO t{table} VALUES ('x', 'p', 1), ('y', 'q', 2);
"
            ));
        }
        connection.execute_batch(&sql).expect("the schema is built");
    }
    write_and_abandon(&path, "ANALYZE;");
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
    let found = read_back(&path, "SELECT b FROM t23 WHERE id = 'y'");
    assert_eq!(found.len(), 1, "the corpus did not survive the replay");
}

/// The same, with an `AUTOINCREMENT` table so `sqlite_sequence` exists.
///
/// The real database has one - `sqlite_sequence`, root 23 - and none of the tests above did. An
/// internal table is exactly the kind of catalog entry a shape derivation is likely to skip, and
/// `ANALYZE` measures every table including that one.
#[test]
fn analyze_survives_a_reopen_with_an_autoincrement_table() {
    let directory = scratch("autoincrement");
    let path = directory.join("stats.db");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session().expect("the connection opens");
        let mut sql = String::from(
            "CREATE TABLE job(id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT);
             INSERT INTO job(kind) VALUES ('one'), ('two');
",
        );
        for table in 0..24 {
            sql.push_str(&format!(
                "CREATE TABLE t{table}(id TEXT PRIMARY KEY, a TEXT, b INT);
"
            ));
            sql.push_str(&format!(
                "CREATE INDEX t{table}_a ON t{table}(a);
"
            ));
            sql.push_str(&format!(
                "INSERT INTO t{table} VALUES ('x', 'p', 1), ('y', 'q', 2);
"
            ));
        }
        connection.execute_batch(&sql).expect("the schema is built");
    }
    write_and_abandon(&path, "ANALYZE;");
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
    let found = read_back(&path, "SELECT kind FROM job WHERE id = 2");
    assert_eq!(found.len(), 1, "the corpus did not survive the replay");
}

/// The distance the parked file's catalog leaf carries past the end of its log.
///
/// Not a round number on purpose: it is the measured one. Page 3 is stamped
/// 21,939,058,496 beside a log that ends at 21,075,008,440.
const MEASURED_DISTANCE: u64 = 864_049_488;

/// Returns the meta record and the page size a database file carries.
///
/// Read out of the file's own bytes rather than through an open, because what
/// these two tests do is put the file into a state an open would refuse to
/// produce - which is the state the parked file is in and no ordinary sequence
/// of statements reaches.
///
/// @param path - the database file
fn meta_of(path: &Path) -> (inillucent_pool::Meta, usize) {
    let bytes = std::fs::read(path).expect("the database file reads");
    let mut size = [0u8; 4];
    size.copy_from_slice(bytes.get(12..16).expect("the page size is in the header"));
    let page_size = u32::from_le_bytes(size) as usize;
    let primary = bytes.get(..page_size).expect("page 0 is there");
    let shadow = bytes
        .get(page_size..page_size * 2)
        .expect("page 1 is there");
    let meta = inillucent_pool::Meta::choose(primary, shadow).expect("the meta record decodes");
    (meta, page_size)
}

/// Writes a meta record over both meta pages, re-checksumming it.
///
/// @param path - the database file
/// @param meta - the record to write
/// @param page_size - the file's page size
fn put_meta(path: &Path, meta: &inillucent_pool::Meta, page_size: usize) {
    use std::io::{Seek, SeekFrom, Write};
    let mut image = vec![0u8; page_size];
    meta.encode(&mut image).expect("the record encodes");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("the database file opens for writing");
    for slot in [0u64, 1] {
        file.seek(SeekFrom::Start(slot * page_size as u64))
            .expect("the meta page is seekable");
        file.write_all(&image).expect("the meta page writes");
    }
}

/// Stamps one page's LSN by hand, leaving the rest of the page alone.
///
/// **The checksum is written again with the stamp**, so the page is byte for
/// byte a valid one. That is the state this file is about: a page carrying a
/// stamp from a stream nobody has, which passes `PRAGMA integrity_check`
/// because nothing about it is damaged. Until task-2074 the checksum left the
/// LSN out and writing the eight bytes alone produced that page; the checksum
/// covers the LSN now (task-2066 section 4.2, item 17), so a stamp written
/// without it is a damaged page instead, which is a different test.
///
/// @param path - the database file
/// @param page - the page to stamp
/// @param page_size - the file's page size
/// @param lsn - the stamp to write
fn stamp_page(path: &Path, page: u64, page_size: usize, lsn: u64) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("the database file opens for writing");
    let mut bytes = vec![0u8; page_size];
    file.seek(SeekFrom::Start(page * page_size as u64))
        .expect("the page is seekable");
    file.read_exact(&mut bytes).expect("the page reads");
    inillucent_pool::page::set_lsn(&mut bytes, lsn).expect("the stamp fits");
    inillucent_pool::page::checksum_page(&mut bytes).expect("the checksum fits");
    file.seek(SeekFrom::Start(page * page_size as u64))
        .expect("the page is seekable");
    file.write_all(&bytes).expect("the stamp writes");
}

/// Builds a schema and closes tidily, so the log holds only the checkpoint.
///
/// @param path - the database file
fn build_and_close(path: &Path) {
    let database = Database::open(path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    let mut sql = String::new();
    for table in 0..8 {
        sql.push_str(&format!(
            "CREATE TABLE t{table}(id TEXT PRIMARY KEY, a TEXT);\n"
        ));
        sql.push_str(&format!(
            "INSERT INTO t{table} VALUES ('x', 'p'), ('y', 'q');\n"
        ));
    }
    connection.execute_batch(&sql).expect("the schema is built");
    // **Explicitly**, because dropping the handle does not checkpoint and a
    // file whose `checkpoint_lsn` is still zero replays its whole log at the
    // next open - which would refuse on the schema's own records rather than on
    // the one write these tests are about.
    database.checkpoint().expect("the file is checkpointed");
}

/// A page stamped above the log's end refuses the open instead of losing the write.
///
/// **This is the reduction the module comment above says does not exist.** The
/// earlier attempt tried to reproduce the *conditions* that set the distance
/// between the resumed position and the stale stamps - build, checkpoint, steal
/// pages with a 64-frame pool, truncate the log - and could not, because the
/// page it wrote reached the file again before the next open and the log was
/// never asked. Stamping the page directly removes that variable: the state is
/// eight bytes, they are outside the page checksum, and the file is otherwise
/// exactly what the engine wrote.
///
/// Without the check in `inillucent-wal`'s replay this open succeeds and the
/// `CREATE TABLE` that was committed a moment earlier is simply not there.
#[test]
fn a_page_stamped_above_the_logs_end_refuses_the_open() {
    let directory = scratch("stamped-above-the-log");
    let path = directory.join("stamped.db");
    build_and_close(&path);

    // The catalog leaf, which is the page the real failure was on: `ANALYZE`
    // wrote `sqlite_stat1`'s catalog row into it and the row was discarded.
    let (meta, page_size) = meta_of(&path);
    let stamp = meta.checkpoint_lsn.saturating_add(MEASURED_DISTANCE);
    stamp_page(&path, meta.catalog_root.0, page_size, stamp);

    // A committed write into that page, with the log left unfolded.
    write_and_abandon(&path, "CREATE TABLE later(a TEXT);");

    let error = match Database::open(&path) {
        Ok(database) => {
            let connection = database.session().expect("the connection opens");
            let rows = connection.query("SELECT count(*) FROM later");
            panic!(
                "the open accepted a file stamped by a stream it does not have, and the \
                 committed CREATE TABLE read back as {rows:?} - which is the silent loss"
            );
        }
        Err(error) => error,
    };
    let detail = error.detail().unwrap_or_default().to_string();
    assert!(
        detail.contains(&format!("page {}", meta.catalog_root.0)),
        "the refusal does not name the page: {detail}"
    );
    assert!(
        detail.contains(&stamp.to_string()),
        "the refusal does not name the stamp: {detail}"
    );
    assert!(
        detail.contains("at or above the log's end"),
        "the refusal does not say why: {detail}"
    );
}

/// A log whose recovered position is below the file's high water resumes above it.
///
/// The prevention half. The file is put into the state the parked one reached -
/// a page stamped past the end of the log beside it - and the meta page carries
/// the high water a checkpoint would have recorded, which is what a file written
/// by this build has and the parked one does not. The open raises the log above
/// the stamp, so the write that follows takes an LSN the page-LSN rule can
/// compare against, and it survives the next open.
///
/// Without the resume the write lands below the stamp and the next open discards
/// it - the same silent loss, on a file whose meta page said enough to prevent
/// it.
#[test]
fn a_log_below_the_files_high_water_resumes_above_it() {
    let directory = scratch("resume-above-the-high-water");
    let path = directory.join("resumed.db");
    build_and_close(&path);

    let (mut meta, page_size) = meta_of(&path);
    let stamp = meta.checkpoint_lsn.saturating_add(MEASURED_DISTANCE);
    stamp_page(&path, meta.catalog_root.0, page_size, stamp);
    // What a checkpoint on this build would have recorded before the segments
    // were moved aside: the highest stamp the file carries.
    meta.high_water_lsn = stamp;
    put_meta(&path, &meta, page_size);

    // The open resumes above the stamp, so this record's LSN is above it too.
    write_and_abandon(&path, "CREATE TABLE later(a TEXT);");

    let rows = read_back(&path, "SELECT count(*) FROM later");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(0),
        "the committed CREATE TABLE did not survive the reopen"
    );
    // And the file now says so: the checkpoint the resume wrote puts the log's
    // position above every stamp the file carries, so this cannot recur.
    let (after, _) = meta_of(&path);
    assert!(
        after.checkpoint_lsn > stamp,
        "the meta still points below the stamp: checkpoint_lsn {} against a stamp of {stamp}",
        after.checkpoint_lsn
    );
    // **At or above the stamp, rather than exactly it.** What matters is that
    // the number never goes backwards: a file whose recorded high water is
    // below a stamp its pages carry is the state this whole case is about. It
    // used to be exactly `stamp` because the resume's checkpoint was the only
    // one a run took; with `locking_mode = normal` as the default (task-1980) a
    // statement that wrote checkpoints on its way out, and those pages carry
    // stamps of their own above the resumed position.
    assert!(
        after.high_water_lsn >= stamp,
        "the high water went backwards through the resume's own checkpoint: {} against a stamp of {stamp}",
        after.high_water_lsn
    );

    // **Once, and then again.** The resume opens a new segment and leaves a gap
    // behind it, so the second cycle is what says the next recovery starts
    // inside that segment rather than stopping at the gap - and that the
    // retirement of the segments below it did not take one the replay needed.
    write_and_abandon(&path, "INSERT INTO later VALUES ('after the resume');");
    let rows = read_back(&path, "SELECT a FROM later");
    assert_eq!(rows.len(), 1, "the second write did not survive its reopen");
    let rows = read_back(&path, "SELECT count(*) FROM t7");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(2),
        "the rows the file already held did not survive the resume"
    );
}
