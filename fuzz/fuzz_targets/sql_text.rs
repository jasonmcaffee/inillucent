//! Fuzzes `Connection::prepare` with arbitrary text.
//!
//! Invariant: **every statement a caller can write is refused or compiled, and
//! never takes the process down.** SQL text is the largest untrusted input this
//! product has: it arrives from an application, from an agent over MCP, from a
//! `.read` of a file, and from a migration's own schema, and everything it
//! reaches - the lexer, the parser, the binder, the planner and the physical
//! translation - is this workspace's own code (task-2066 section 4.4.7).
//!
//! It is `prepare` rather than `parse` on purpose. The parser has a seeded
//! counterpart already; what only a prepare reaches is the binder resolving
//! names against a catalog, the planner choosing between them, and the physical
//! pass building operators - and one of task-2066's own release blockers was a
//! recursion in a parser reached from an MCP request, so the layers above it
//! are where the untested depth is.
//!
//! **One database per thread, opened once.** Building a fresh one per input
//! would spend almost every cycle in `CREATE TABLE` and almost none in the code
//! under test, which is the shape that makes a fuzz target look busy and find
//! nothing. The schema is small and deliberately awkward: two tables that share
//! a column name, an index, a view over a join, and a trigger, so a bound name
//! can resolve several ways and a statement can reach the trigger planner.
//!
//! A `thread_local!` rather than a `OnceLock`, because a `Database` holds its
//! engine in a `RefCell` and is not `Sync`: a `OnceLock` would not compile, and
//! it should not - a connection is single threaded here and sharing one across
//! libfuzzer's workers would be a race the target invented rather than one the
//! engine has.

#![no_main]

use std::cell::OnceCell;

use libfuzzer_sys::fuzz_target;

use inillucent_engine::connect::Database;

/// The schema every input is prepared against.
const SCHEMA: &str = "\
CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT, team TEXT);\
CREATE TABLE b (id INTEGER PRIMARY KEY, team TEXT, rank INTEGER);\
CREATE INDEX a_team ON a (team);\
CREATE VIEW both AS SELECT a.name, b.rank FROM a JOIN b ON a.team = b.team;\
CREATE TRIGGER a_ins AFTER INSERT ON a BEGIN UPDATE b SET rank = rank + 1; END;";

thread_local! {
    /// The one database this thread's inputs are prepared against.
    ///
    /// Leaked on purpose: a fuzz target has no teardown, and holding it for the
    /// life of the process is what keeps the cost of a database out of the
    /// per-input path.
    static HELD: OnceCell<&'static Database> = const { OnceCell::new() };
}

/// Returns the one database this thread's inputs are prepared against.
fn database() -> &'static Database {
    HELD.with(|held| {
        *held.get_or_init(|| {
            let directory = std::env::temp_dir().join(format!(
                "inillucent-fuzz-sql-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::create_dir_all(&directory);
            let database = Database::open(directory.join("fuzz.rdb")).expect("the database opens");
            let leaked: &'static Database = Box::leak(Box::new(database));
            leaked
                .session()
                .execute_batch(SCHEMA)
                .expect("the schema is created");
            leaked
        })
    })
}

fuzz_target!(|data: &[u8]| {
    // Not every byte string is text, and a statement that is not UTF-8 is a
    // different question - `cli_import` asks it. Lossy would invent characters
    // the caller never wrote, so an input that is not text is skipped.
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let connection = database().session();
    // The statement is only prepared, never stepped. Running it would spend the
    // budget in the storage engine, which the page and log targets already
    // cover, and would let one input change the schema the next one is
    // prepared against - so the corpus would stop being reproducible.
    let _ = connection.prepare(text);
});
