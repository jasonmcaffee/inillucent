//! Cross-mutation: each engine writes a file the other one then opens, checks,
//! and writes to in turn.
//!
//! Invariant: a file is a contract, not a private format. A reader that accepts
//! only what its own writer produces has tested nothing - the two agree because
//! they are the same code - so every claim here is made by the *other* engine.
//! inillucent's work is judged by SQLite's `PRAGMA integrity_check` and by what
//! SQLite reads back, and SQLite's work is judged by inillucent's own raw
//! traversal.
//!
//! What is deliberately not claimed is byte-for-byte identity. Two valid B-tree
//! layouts differ - a different split point is not a different database - so
//! what has to match is the logical contents, the structural checks, and the
//! ability of each engine to keep writing after the other one has.
//!
//! The tables inillucent writes into are ones it creates itself, and that is not
//! laziness. Adding a row to an indexed table without also adding it to every
//! index leaves a database that is structurally perfect and logically wrong,
//! and SQLite says so. Which columns an index covers, and under which
//! collation, is written in SQL - which the storage layer cannot read, and will
//! not pretend to. Keeping index maintenance above storage is the layering, so
//! the interop a storage phase can honestly claim is over trees it owns
//! entirely.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_compat::{fixtures, workspace_root};
use inillucent_storage::check::{self, CheckOptions};
use inillucent_storage::cursor::BTreeCursor;
use inillucent_storage::header::VacuumMode;
use inillucent_storage::pager::{NewDatabase, Pager, PagerOptions};
use inillucent_storage::{mutate, schema};
use inillucent_value::record::{encode_record, RecordRef};
use inillucent_value::{BlobValue, TextEncoding, Value};
use inillucent_vfs::{DbPath, OsVfs};

/// The name of the table inillucent creates in a file SQLite wrote.
const ADDED_TABLE: &str = "inillucent_added";

/// The SQL that describes it, which is what goes in `sqlite_schema`.
const ADDED_SQL: &str = "CREATE TABLE inillucent_added(a)";

/// Returns the pinned SQLite shell, or `None` when it has not been downloaded.
fn pinned_shell() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_SHELL") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    // The name to try first is the one this platform runs. Both are on disk
    // when the workspace is shared between Windows and WSL, and a Linux process
    // that picks the `.exe` gets a *Windows* SQLite through binfmt interop -
    // which then cannot open a Linux path, and says "unable to open database"
    // for a reason that has nothing to do with the database.
    let names: [&str; 2] = if cfg!(windows) {
        ["sqlite3.exe", "sqlite3"]
    } else {
        ["sqlite3", "sqlite3.exe"]
    };
    for name in names {
        let path = directory.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    eprintln!("the pinned SQLite shell is not present; run tools/sqlite-reference.{{ps1,sh}}");
    None
}

/// Runs the pinned shell against a database and returns its output.
fn run_sqlite(shell: &Path, database: &Path, sql: &str) -> Result<String, String> {
    let output = Command::new(shell)
        .arg(database)
        .arg(sql)
        .output()
        .map_err(|error| format!("cannot run {}: {error}", shell.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() || !stderr.trim().is_empty() {
        return Err(format!(
            "sqlite3 exited with {:?}\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        ));
    }
    Ok(stdout)
}

/// Returns a fresh directory for one test's databases.
///
/// The operating system's temporary directory rather than the workspace, and
/// that is not a preference. SQLite locks a database with POSIX advisory locks,
/// and a Linux process cannot take one on a file under a Windows drive mount -
/// the shell fails to open the database at all. A test that put its scratch in
/// `target/` therefore passed on Windows and failed under WSL for a reason that
/// had nothing to do with either engine.
fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join("inillucent-interop").join(name);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Builds the record one added row holds: a single blob of `len` bytes derived
/// from the rowid, so a swapped row is not mistaken for the right one.
fn added_row(rowid: i64, len: usize) -> Vec<u8> {
    let filler: Vec<u8> = (0..len)
        .map(|index| (rowid as u8).wrapping_mul(31).wrapping_add(index as u8))
        .collect();
    encode_record(
        &[Value::Blob(BlobValue::borrowed(&filler))],
        TextEncoding::Utf8,
        4,
    )
    .expect("a record")
}

/// Returns the hex SQLite's `quote()` prints for a blob of the same shape.
fn quoted(rowid: i64, len: usize) -> String {
    let filler: Vec<u8> = (0..len)
        .map(|index| (rowid as u8).wrapping_mul(31).wrapping_add(index as u8))
        .collect();
    let mut text = String::with_capacity(len * 2 + 3);
    text.push_str("X'");
    for byte in filler {
        text.push_str(&format!("{byte:02X}"));
    }
    text.push('\'');
    text
}

/// The rowids and payload sizes every added table is populated with.
///
/// The sizes straddle the local-payload threshold at every page size in the
/// corpus, so at least one row on every file overflows and at least one does
/// not, without the test having to know which page size it is looking at.
fn added_rows() -> Vec<(i64, usize)> {
    let mut rows = Vec::new();
    for index in 0..60i64 {
        let len = match index % 6 {
            0 => 8,
            1 => 100,
            2 => 480,
            3 => 1_100,
            4 => 5_000,
            _ => 60,
        };
        rows.push((index.saturating_add(1), len));
    }
    rows
}

/// Adds a table to a database and fills it, entirely through inillucent.
///
/// The `sqlite_schema` row is written the same way any other row is: it is a
/// five-field record in a table B-tree, and the fourth field is the root page.
/// Nothing here parses the SQL it stores - it is a string this test chose, and
/// SQLite is the one that will read it.
fn add_table_with_inillucent(path: &Path, rows: &[(i64, usize)]) -> Result<(), String> {
    let vfs = OsVfs::new();
    let db_path = DbPath::new(path);
    let mut pager = Pager::open_read_write(&vfs, &db_path, PagerOptions::default())
        .map_err(|error| format!("{error:?}"))?;
    pager.begin_write().map_err(|error| format!("{error:?}"))?;

    let root = mutate::create_table(&mut pager).map_err(|error| format!("{error:?}"))?;
    for (rowid, len) in rows {
        mutate::insert_row(&mut pager, root, *rowid, &added_row(*rowid, *len))
            .map_err(|error| format!("{error:?}"))?;
    }
    // Delete a third of them again so the file carries a freelist inillucent built.
    for (rowid, _) in rows.iter().filter(|(rowid, _)| rowid % 3 == 0) {
        mutate::delete_row(&mut pager, root, *rowid).map_err(|error| format!("{error:?}"))?;
    }

    let schema_root = PageId::from_persisted(schema::SCHEMA_ROOT).map_err(|e| format!("{e:?}"))?;
    let next_rowid = highest_schema_rowid(&mut pager)?.saturating_add(1);
    let encoding = pager.text_encoding();
    let record = encode_record(
        &[
            Value::text_utf8(b"table"),
            Value::text_utf8(ADDED_TABLE.as_bytes()),
            Value::text_utf8(ADDED_TABLE.as_bytes()),
            Value::Integer(i64::from(root.get())),
            Value::text_utf8(ADDED_SQL.as_bytes()),
        ],
        encoding,
        4,
    )
    .map_err(|error| format!("{error:?}"))?;
    mutate::insert_row(&mut pager, schema_root, next_rowid, &record)
        .map_err(|error| format!("{error:?}"))?;

    // The schema cookie has to move, or a connection that had already read the
    // schema would keep using the old one and never see the new table.
    let mut header = *pager.header();
    header.schema_cookie = header.schema_cookie.wrapping_add(1);
    pager
        .set_header(header)
        .map_err(|error| format!("{error:?}"))?;

    pager.commit().map_err(|error| format!("{error:?}"))?;
    Ok(())
}

/// Returns the largest rowid in `sqlite_schema`, or zero when it is empty.
fn highest_schema_rowid(pager: &mut Pager) -> Result<i64, String> {
    let schema_root = PageId::from_persisted(schema::SCHEMA_ROOT).map_err(|e| format!("{e:?}"))?;
    let mut cursor = BTreeCursor::table(schema_root);
    if !cursor.last(pager).map_err(|error| format!("{error:?}"))? {
        return Ok(0);
    }
    cursor.rowid().map_err(|error| format!("{error:?}"))
}

/// Reads a table's rows back through inillucent as `(rowid, payload)`.
fn read_table(pager: &mut Pager, root: PageId) -> Result<Vec<(i64, Vec<u8>)>, String> {
    let limits = Limits::default();
    let mut cursor = BTreeCursor::table(root);
    let mut rows = Vec::new();
    let mut more = cursor.first(pager).map_err(|error| format!("{error:?}"))?;
    while more {
        let rowid = cursor.rowid().map_err(|error| format!("{error:?}"))?;
        let payload = cursor
            .payload(pager, &limits)
            .map_err(|error| format!("{error:?}"))?;
        rows.push((rowid, payload));
        more = cursor.next(pager).map_err(|error| format!("{error:?}"))?;
    }
    Ok(rows)
}

/// Runs inillucent's own integrity check over a whole database.
fn inillucent_integrity(path: &Path) -> Result<(), String> {
    let vfs = OsVfs::new();
    let mut pager = Pager::open_read_only(&vfs, &DbPath::new(path), PagerOptions::default())
        .map_err(|error| format!("{error:?}"))?;
    pager.begin_read().map_err(|error| format!("{error:?}"))?;
    let report = check::check_database_with_options(&mut pager, &CheckOptions::integrity())
        .map_err(|error| format!("{error:?}"))?;
    if report.is_ok() {
        return Ok(());
    }
    Err(format!("{:?}", report.as_pragma_output()))
}

/// SQLite writes a file, inillucent adds a table to it, and SQLite checks and
/// reads the result.
#[test]
fn inillucent_mutates_a_sqlite_file_and_sqlite_accepts_it() {
    let Some(shell) = pinned_shell() else { return };
    let corpus = fixtures::corpus_root(&workspace_root());
    let directory = scratch("inillucent-writes");
    let names = [
        "basic-p512-utf8.db",
        "basic-p1024-utf8.db",
        "basic-p4096-utf8.db",
        "basic-p65536-utf8.db",
        "deep-p512-utf8.db",
        "freelist-p1024-utf8.db",
        "overflow-p512-utf8.db",
        "reserved-p4096-utf8.db",
        "autovacuum-p1024-utf8.db",
        "incrvacuum-p1024-utf8.db",
        "collations-p1024-utf8.db",
    ];
    let rows = added_rows();
    let expected: Vec<String> = rows
        .iter()
        .filter(|(rowid, _)| rowid % 3 != 0)
        .map(|(rowid, len)| quoted(*rowid, *len))
        .collect();

    for name in names {
        let source = corpus.join(name);
        if !source.is_file() {
            eprintln!("the fixture corpus is not built; run inillucent-fixtures");
            return;
        }
        let target = directory.join(name);
        std::fs::copy(&source, &target).expect("a copy of the fixture");

        // SQLite says the file is sound before inillucent touches it.
        let before = run_sqlite(&shell, &target, "PRAGMA integrity_check;")
            .unwrap_or_else(|reason| panic!("{name}: {reason}"));
        assert_eq!(before.trim(), "ok", "{name} was not sound to begin with");

        add_table_with_inillucent(&target, &rows)
            .unwrap_or_else(|reason| panic!("{name}: {reason}"));

        let after = run_sqlite(&shell, &target, "PRAGMA integrity_check;")
            .unwrap_or_else(|reason| panic!("{name}: {reason}"));
        assert_eq!(
            after.trim(),
            "ok",
            "{name}: SQLite refused the file inillucent wrote"
        );

        let listed = run_sqlite(
            &shell,
            &target,
            &format!("SELECT quote(a) FROM {ADDED_TABLE} ORDER BY rowid;"),
        )
        .unwrap_or_else(|reason| panic!("{name}: {reason}"));
        let found: Vec<String> = listed.lines().map(str::to_string).collect();
        assert_eq!(found, expected, "{name}: SQLite read back different rows");

        // The tables SQLite wrote are still there and still readable.
        let people = run_sqlite(
            &shell,
            &target,
            "SELECT count(*) FROM sqlite_schema WHERE type='table';",
        )
        .unwrap_or_else(|reason| panic!("{name}: {reason}"));
        assert!(
            people.trim().parse::<i64>().unwrap_or(0) >= 2,
            "{name}: the original tables went missing"
        );
    }
}

/// inillucent writes a whole database from nothing, and SQLite opens it, checks
/// it, reads it, writes to it, and checks it again - after which inillucent reads
/// what SQLite wrote.
#[test]
fn sqlite_mutates_a_inillucent_file_and_inillucent_accepts_it() {
    let Some(shell) = pinned_shell() else { return };
    let directory = scratch("sqlite-writes");
    let rows = added_rows();

    for (page_size, vacuum) in [
        (512u32, VacuumMode::None),
        (1024, VacuumMode::None),
        (4096, VacuumMode::None),
        (65_536, VacuumMode::None),
        (1024, VacuumMode::Incremental),
    ] {
        let label = format!("p{page_size}-{vacuum:?}");
        let path = directory.join(format!("{label}.db"));
        let vfs = OsVfs::new();
        let db_path = DbPath::new(&path);
        {
            let mut pager = Pager::create(
                &vfs,
                &db_path,
                PagerOptions::default(),
                NewDatabase {
                    page_size: inillucent_base::page::PageSize::new(page_size)
                        .expect("a page size"),
                    reserved_bytes: 0,
                    text_encoding: TextEncoding::Utf8,
                    vacuum_mode: vacuum,
                },
            )
            .unwrap_or_else(|error| panic!("{label}: {error:?}"));
            pager.commit().ok();
        }
        add_table_with_inillucent(&path, &rows)
            .unwrap_or_else(|reason| panic!("{label}: {reason}"));

        let checked = run_sqlite(&shell, &path, "PRAGMA integrity_check;")
            .unwrap_or_else(|reason| panic!("{label}: {reason}"));
        assert_eq!(
            checked.trim(),
            "ok",
            "{label}: SQLite refused a database inillucent created from nothing"
        );

        // SQLite now writes to it: new rows, an index over the whole table, and
        // deletes that put pages back on the freelist inillucent built.
        let written = run_sqlite(
            &shell,
            &path,
            &format!(
                "INSERT INTO {ADDED_TABLE}(rowid, a) \
                 SELECT 1000 + value, randomblob(1 + (value * 37) % 4000) \
                 FROM generate_series(1, 200);\n\
                 CREATE INDEX added_by_a ON {ADDED_TABLE}(a);\n\
                 DELETE FROM {ADDED_TABLE} WHERE rowid % 5 = 0;\n\
                 PRAGMA integrity_check;"
            ),
        )
        .unwrap_or_else(|reason| panic!("{label}: {reason}"));
        assert_eq!(
            written.trim(),
            "ok",
            "{label}: SQLite could not write to the file it had just checked"
        );

        // inillucent reads what SQLite wrote, and its own traversal agrees.
        inillucent_integrity(&path).unwrap_or_else(|reason| panic!("{label}: {reason}"));

        let counted = run_sqlite(
            &shell,
            &path,
            &format!("SELECT count(*) FROM {ADDED_TABLE};"),
        )
        .unwrap_or_else(|reason| panic!("{label}: {reason}"));
        let expected: i64 = counted.trim().parse().unwrap_or(-1);

        let mut pager = Pager::open_read_only(&vfs, &db_path, PagerOptions::default())
            .unwrap_or_else(|error| panic!("{label}: {error:?}"));
        pager.begin_read().expect("a read transaction");
        let object = schema::find_object(&mut pager, ADDED_TABLE)
            .unwrap_or_else(|error| panic!("{label}: {error:?}"))
            .unwrap_or_else(|| panic!("{label}: the table inillucent created is gone"));
        let root = object.root_page.expect("a root page");
        let found =
            read_table(&mut pager, root).unwrap_or_else(|reason| panic!("{label}: {reason}"));
        assert_eq!(
            found.len() as i64,
            expected,
            "{label}: inillucent and SQLite disagree about how many rows there are"
        );
        for (rowid, payload) in &found {
            RecordRef::parse_with_limits(payload, TextEncoding::Utf8, &Limits::default())
                .unwrap_or_else(|error| panic!("{label}: row {rowid}: {error:?}"));
        }
    }
}

/// A file each engine writes to in turn, three times round, stays sound to
/// both.
///
/// One pass proves the format is readable. Alternating proves neither engine
/// depends on the other's *layout* - that inillucent can balance a page SQLite
/// split, and that SQLite can insert into a page inillucent laid out its own way.
#[test]
fn the_two_engines_take_turns_writing_the_same_file() {
    let Some(shell) = pinned_shell() else { return };
    let directory = scratch("turns");
    let path = directory.join("turns.db");
    let vfs = OsVfs::new();
    let db_path = DbPath::new(&path);
    {
        let mut pager = Pager::create(
            &vfs,
            &db_path,
            PagerOptions::default(),
            NewDatabase {
                page_size: inillucent_base::page::PageSize::new(1024).expect("a page size"),
                reserved_bytes: 0,
                text_encoding: TextEncoding::Utf8,
                vacuum_mode: VacuumMode::None,
            },
        )
        .expect("a new database");
        pager.commit().ok();
    }
    add_table_with_inillucent(&path, &added_rows()).expect("inillucent writes first");

    for round in 0..3 {
        // A base that cannot collide with the rows already there, or with the
        // ones inillucent adds below: each engine writes into its own band.
        let base = (round + 1) * 100_000;
        let written = run_sqlite(
            &shell,
            &path,
            &format!(
                "INSERT INTO {ADDED_TABLE}(rowid, a) \
                 SELECT {base} + value, randomblob(1 + (value * 13) % 2500) \
                 FROM generate_series(1, 120);\n\
                 DELETE FROM {ADDED_TABLE} WHERE rowid % 7 = {round};\n\
                 PRAGMA integrity_check;"
            ),
        )
        .unwrap_or_else(|reason| panic!("round {round}: {reason}"));
        assert_eq!(written.trim(), "ok", "round {round}: SQLite's own check");
        inillucent_integrity(&path).unwrap_or_else(|reason| panic!("round {round}: {reason}"));

        // inillucent's turn: more rows, and deletes that reuse SQLite's freelist.
        let mut pager =
            Pager::open_read_write(&vfs, &db_path, PagerOptions::default()).expect("a writer");
        pager.begin_write().expect("a write transaction");
        let object = schema::find_object(&mut pager, ADDED_TABLE)
            .expect("a schema read")
            .expect("the table");
        let root = object.root_page.expect("a root page");
        for index in 0..120i64 {
            let rowid = 900_000 + round as i64 * 1_000 + index;
            let len = ((index as usize).saturating_mul(97) % 3_000).saturating_add(1);
            mutate::insert_row(&mut pager, root, rowid, &added_row(rowid, len))
                .unwrap_or_else(|error| panic!("round {round}: {error:?}"));
        }
        for index in (0..120i64).step_by(3) {
            let rowid = 900_000 + round as i64 * 1_000 + index;
            mutate::delete_row(&mut pager, root, rowid)
                .unwrap_or_else(|error| panic!("round {round}: {error:?}"));
        }
        pager
            .commit()
            .unwrap_or_else(|error| panic!("round {round}: {error:?}"));
        drop(pager);

        inillucent_integrity(&path).unwrap_or_else(|reason| panic!("round {round}: {reason}"));
        let checked = run_sqlite(&shell, &path, "PRAGMA integrity_check;")
            .unwrap_or_else(|reason| panic!("round {round}: {reason}"));
        assert_eq!(
            checked.trim(),
            "ok",
            "round {round}: SQLite refused the file after inillucent's turn"
        );
    }
}
