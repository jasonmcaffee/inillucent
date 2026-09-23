//! Fuzzes the SQLite file reader the migration path uses.
//!
//! Invariant: **a file `inillucent-migrate` is pointed at is another program's
//! output, and reading it must refuse or answer, never abort.** It is the one
//! untrusted input that arrives as a whole *file* rather than as a value: a
//! person migrating a database hands over whatever they have, and a file that
//! was truncated by a copy, written by a different version, or was never a
//! SQLite database at all is the ordinary case rather than the hostile one
//! (task-2066 section 4.4.7).
//!
//! **The whole reader, not one decoder.** `corruption.rs` damages a real
//! fixture and reads it through the pager, which is the right shape for asking
//! whether damage is detected; what it cannot reach is a header that makes the
//! pager build itself wrongly before any page is read. So this drives
//! `SqliteFile::open` and then asks for the schema and a table, because the
//! header decides the page size, the page count and where the schema root is,
//! and every later read is computed from those three.
//!
//! The bytes are written to a file because that is what the reader takes. It is
//! the slowest thing a target in this directory does, and it is what the input
//! actually is - a reader that took a slice would be a different function from
//! the one `inillucent-migrate` calls.

#![no_main]

use std::path::PathBuf;

use libfuzzer_sys::fuzz_target;

use inillucent_sqlite_reader::SqliteFile;

/// Returns the one path every input is written to.
///
/// One path rather than a fresh one per input: a fuzzer runs millions of
/// inputs, and a directory with millions of files in it is a different test.
fn scratch() -> PathBuf {
    let directory = std::env::temp_dir().join(format!("inillucent-fuzz-sqlite-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&directory);
    directory.join("input.db")
}

fuzz_target!(|data: &[u8]| {
    // A file shorter than a header is a case the reader has to refuse, and it
    // is cheap, so it is not skipped.
    let path = scratch();
    if std::fs::write(&path, data).is_err() {
        return;
    }
    let Ok(mut file) = SqliteFile::open(path) else {
        return;
    };
    // The header decided these three, so they are read before anything else.
    let _ = file.page_size();
    let _ = file.page_count();
    let Ok(schema) = file.schema() else {
        return;
    };
    // And then one table through the reader the migration uses, because a root
    // page the header pointed at is where a wrong page size first shows.
    for object in schema.iter().take(8).cloned().collect::<Vec<_>>() {
        let columns = object.column_names().map(|names| names.len()).unwrap_or(1);
        let _ = file.read_table(object.root, columns);
    }
});
