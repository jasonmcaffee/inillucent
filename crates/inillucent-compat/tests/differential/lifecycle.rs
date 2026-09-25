//! Statement lifecycle, result metadata and parameters.
//!
//! Invariant: the public API behaves the way SQLite's does, including when it
//! is misused. A statement stepped past its end reports done rather than
//! panicking, and a parameter index that does not exist is a misuse error
//! rather than a silent no-op.
//!
//! **Three tests that used to live here are gone, and are not replaced,
//! because there is nothing left in the new engine to replace them with:**
//!
//! - `an_interrupt_stops_a_running_statement` needed `Connection::interrupt`/
//!   `clear_interrupt`. `inillucent_engine::connect::Connection` has neither, and
//!   nothing else in `inillucent-engine` implements a cross-thread interrupt
//!   under another name - this is a real capability gap, not a test-writing
//!   one, and `compat/sqlite-3.53.4.toml`'s `vm.statement.interrupt` row (which
//!   claims `status = "pass"` on the strength of exactly this one test) needs
//!   a person to look at it: its `tests` array is now empty, which fails
//!   `harness.rs::the_shipped_manifest_is_structurally_sound`.
//! - `the_verifier_rejects_generated_invalid_programs` and
//!   `the_verifier_rejects_mismatched_operands` tested `inillucent_vm::verify`/
//!   `verify_operands` over a hand-built `Program` of `Instruction`s and
//!   `Opcode`s. The new engine compiles to an operator tree
//!   (`inillucent-exec`), not bytecode, and has no verifier of any shape -
//!   `inillucent-exec` was searched for `verify`/`Program` and has neither.
//!   `compat/sqlite-3.53.4.toml`'s `vm.bytecode.verifier` row cites only these
//!   two tests and is now empty for the same reason.
//! - `the_machine_runs_a_verified_program` drove `inillucent_vm::machine::Machine`
//!   directly. There is no `Machine` in the new engine to drive; a statement is
//!   run through `Connection::prepare`/`Statement::step`, which the tests below
//!   already exercise.

use std::path::PathBuf;

use inillucent_compat::differential::tagged;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// Returns the corpus fixture's path.
fn fixture() -> PathBuf {
    workspace_root().join("compat/fixtures/select-corpus.db")
}

/// Returns the corpus fixture rebuilt as a native database, importing it
/// exactly once for the whole test binary.
///
/// **Imported, not opened directly.** The fixture is a SQLite file, and this
/// engine's file format is not SQLite's - `Database::open` on one reports that
/// neither meta page is readable, which is correct and is not what this suite
/// wants. `Database::import` reads it once through `inillucent-sqlite-reader`
/// and rebuilds it as PAX trees at `<fixture>.rdb`, beside the source - one
/// fixed path, so every test in this file that imported it separately (and in
/// parallel, since libtest runs `#[test]`s on their own threads) would have
/// raced rewriting the same file. Importing once, behind a `OnceLock`, and
/// handing every test a plain `Database::open` on the result avoids that.
fn imported_path() -> &'static PathBuf {
    static IMPORTED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    IMPORTED.get_or_init(|| {
        Database::import(fixture())
            .expect("the fixture imports")
            .path()
            .to_path_buf()
    })
}

/// Opens a connection onto the corpus fixture.
fn connect() -> Connection<'static> {
    let database: &'static Database = Box::leak(Box::new(
        Database::open(imported_path()).expect("the import opens"),
    ));
    database.session()
}

/// A statement steps to done and then keeps reporting done.
#[test]
fn a_statement_steps_to_done_and_stays_there() {
    let connection = connect();
    let mut statement = connection
        .prepare("SELECT id FROM people WHERE id = 1")
        .expect("it prepares");
    assert!(statement.step().expect("it steps"));
    assert!(!statement.step().expect("it steps"));
    assert!(!statement.step().expect("it steps"));
    assert!(!statement.step().expect("it steps"));
}

/// A statement reset runs again from the beginning, keeping its bindings.
#[test]
fn reset_runs_again_and_keeps_bindings() {
    let connection = connect();
    let mut statement = connection
        .prepare("SELECT name FROM people WHERE id = ?1")
        .expect("it prepares");
    statement.bind_integer(1, 1).expect("it binds");
    assert!(statement.step().expect("it steps"));
    let first = statement.row().first().cloned();
    assert!(!statement.step().expect("it steps"));

    statement.reset();
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.row().first().cloned(), first);

    statement.reset();
    statement.clear_bindings();
    // With the binding cleared the parameter is NULL, and `id = NULL` is never
    // true, so the statement returns nothing rather than failing.
    assert!(!statement.step().expect("it steps"));
}

/// An out-of-range parameter index is reported as `Range`, not a silent no-op.
///
/// This used to assert `Misuse` for both the too-high index and the
/// below-range zero one, which is not what SQLite itself does: its own
/// `vdbeUnbind` (`.sqlite-ref/3.53.4/src/sqlite3.c`) reserves `SQLITE_MISUSE`
/// for a statement that is busy or already finalized, and answers a plain
/// out-of-range index - whichever direction, since the 1-based index arrives
/// there already converted to a 0-based one that underflows for `0` - with
/// `SQLITE_RANGE`. Neither case here is a name that fails to resolve; both
/// are a positional index outside `[1, nVar]`, which is exactly the case the
/// C API docs for `sqlite3_bind_*` name: "If the second parameter to these
/// routines is out of range, then SQLITE_RANGE is returned."
#[test]
fn an_out_of_range_parameter_index_is_a_range_error() {
    let connection = connect();
    let mut statement = connection.prepare("SELECT ?1").expect("it prepares");
    assert!(statement.bind_integer(1, 1).is_ok());
    let failure = statement
        .bind_integer(2, 1)
        .expect_err("parameter 2 does not exist");
    assert_eq!(failure.code(), inillucent_base::PrimaryCode::Range);
    let zero = statement
        .bind_integer(0, 1)
        .expect_err("parameters are one-based");
    assert_eq!(zero.code(), inillucent_base::PrimaryCode::Range);
}

/// Every bindable class round-trips through a parameter.
#[test]
fn every_class_binds_and_returns() {
    let connection = connect();
    let mut statement = connection
        .prepare("SELECT ?1, ?2, ?3, ?4, ?5")
        .expect("it prepares");
    statement.bind(1, OwnedDatum::Null).expect("it binds");
    statement.bind_integer(2, -7).expect("it binds");
    statement.bind(3, OwnedDatum::Real(1.5)).expect("it binds");
    statement.bind_text(4, "text").expect("it binds");
    statement.bind_blob(5, &[0x00, 0xff]).expect("it binds");
    assert!(statement.step().expect("it steps"));
    let row = statement.row();
    assert_eq!(row.first(), Some(&OwnedDatum::Null));
    assert_eq!(row.get(1), Some(&OwnedDatum::Int(-7)));
    assert_eq!(row.get(2), Some(&OwnedDatum::Real(1.5)));
    assert_eq!(row.get(3), Some(&OwnedDatum::Text(b"text".to_vec())));
    assert_eq!(row.get(4), Some(&OwnedDatum::Blob(vec![0x00, 0xff])));
}

/// A connection is in autocommit until a transaction is opened on it.
///
/// This used to also assert that a prepared `SELECT` is read-only;
/// `inillucent_engine::connect::Statement` has no `is_readonly` of any kind,
/// which is a smaller gap than the interrupt/verifier ones above (nothing
/// downstream depends on the flag existing) but is still a capability the old
/// engine had and the new one does not expose.
#[test]
fn the_connection_is_in_autocommit() {
    let connection = connect();
    assert!(connection
        .autocommit()
        .expect("nothing is running on this connection"));
    let _ = connection.prepare("SELECT 1").expect("it prepares");
}

/// Prepare reports the tail so a caller can walk a script.
#[test]
fn prepare_reports_the_tail() {
    let connection = connect();
    let sql = "SELECT 1; SELECT 2;";
    let first = connection.prepare_with_tail(sql).expect("it prepares");
    let consumed = first.consumed;
    let mut first = first.statement;
    assert!(first.step().expect("it steps"));
    assert_eq!(first.row().first(), Some(&OwnedDatum::Int(1)));
    let rest = sql.get(consumed..).unwrap_or("");
    let mut second = connection
        .prepare_with_tail(rest)
        .expect("it prepares")
        .statement;
    assert!(second.step().expect("it steps"));
    assert_eq!(second.row().first(), Some(&OwnedDatum::Int(2)));
}

/// Column metadata is available only **after** the first step, and the names
/// it then gives are the ones a caller reads results by.
///
/// **The "before" half of this test is a recorded difference from SQLite**, and
/// the engine states it itself, on `Statement::columns`: "Empty before the
/// first `step`, which is where this differs from `sqlite3_column_name`: that
/// answers straight after a prepare, because SQLite compiles the column names
/// as part of compiling the statement. This statement materialises on its first
/// step and learns its shape from what came back, so there is nothing to report
/// until then. A caller that asked first got an empty list and printed no
/// header, which is how the difference was found."
///
/// So the empty answer is asserted rather than the three names, and the three
/// names are asserted after a step. A caller binding a result set before
/// running it - which is what this test used to be named for - cannot do that
/// here, and an engine that later compiles the names at prepare time turns the
/// first assertion red, which is the point of keeping it.
#[test]
fn column_metadata_is_available_after_stepping() {
    let connection = connect();
    let mut statement = connection
        .prepare("SELECT id, name AS who, id + 1 FROM people")
        .expect("it prepares");
    assert_eq!(
        statement.columns().len(),
        0,
        "this engine reports no column names until the first step; if it now \
         reports three, `Statement::columns`'s own doc comment is out of date \
         and this test should assert the names here instead"
    );
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.columns().len(), 3);
    assert_eq!(statement.columns().first().map(String::as_str), Some("id"));
    assert_eq!(statement.columns().get(1).map(String::as_str), Some("who"));
    // An expression with no alias is named after the text it was written as,
    // which is what SQLite's default `short_column_names` produces and what an
    // application reading results by name depends on.
    assert_eq!(
        statement.columns().get(2).map(String::as_str),
        Some("id + 1")
    );
    assert_eq!(statement.columns().get(3), None);
}

/// Reading a database changes nothing about it, even after many statements.
#[test]
fn a_long_session_changes_no_byte() {
    let path = imported_path().clone();
    let database = Database::open(&path).expect("the import opens");
    let before = std::fs::read(&path).expect("the database reads");
    let connection = database.session();
    for sql in [
        "SELECT count(*) FROM people",
        "SELECT * FROM people ORDER BY name",
        "SELECT team, count(*) FROM people GROUP BY team",
        "SELECT DISTINCT team FROM people",
    ] {
        let _ = connection.query(sql).expect("it runs");
    }
    let after = std::fs::read(&path).expect("the database reads");
    assert_eq!(before, after);
}

/// Two statements on one connection can be stepped alternately and see the
/// same snapshot, which is what the reference-counted read transaction is for.
#[test]
fn two_statements_interleave_on_one_connection() {
    let connection = connect();
    let mut first = connection
        .prepare("SELECT id FROM people ORDER BY id")
        .expect("it prepares");
    let mut second = connection
        .prepare("SELECT id FROM people ORDER BY id DESC")
        .expect("it prepares");
    let mut ascending = Vec::new();
    let mut descending = Vec::new();
    loop {
        let more_first = first.step().expect("it steps");
        let more_second = second.step().expect("it steps");
        if more_first {
            ascending.push(first.row().first().cloned());
        }
        if more_second {
            descending.push(second.row().first().cloned());
        }
        if !more_first && !more_second {
            break;
        }
    }
    descending.reverse();
    assert_eq!(ascending, descending);
    assert_eq!(ascending.len(), 10);
}

/// Returns the pinned oracle, if it has been built.
fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// The metadata, autocommit flag and bound values match the pinned release.
#[test]
fn lifecycle_and_metadata_match_the_oracle() {
    let Some(program) = sqlite_oracle() else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("it answers");
    driver
        .send(&Op::Open(fixture().display().to_string()))
        .expect("it opens");
    let connection = connect();

    // Column names, for the three forms SQLite names differently.
    //
    // **Stepped before the names are read**, because this engine has none
    // until then - see `column_metadata_is_available_after_stepping` above and
    // `Statement::columns`'s own doc comment. The oracle answers straight after
    // a prepare, so the comparison is of what each engine reports once it has
    // produced a row, which is the point at which both agree and the point an
    // application reads a result by name.
    for (sql, expected) in [
        ("SELECT id FROM people", vec!["id"]),
        ("SELECT id AS x FROM people", vec!["x"]),
        ("SELECT id, name FROM people", vec!["id", "name"]),
    ] {
        let observation = driver.send(&Op::Query(sql.to_string())).expect("it runs");
        let mut statement = connection.prepare(sql).expect("it prepares");
        assert!(statement.step().expect("it steps"), "{sql} produced no row");
        let ours: Vec<String> = statement.columns().to_vec();
        assert_eq!(observation.columns, expected, "{sql}");
        assert_eq!(ours, expected, "{sql}");
        assert!(observation.autocommit);
        assert_eq!(
            connection
                .autocommit()
                .expect("nothing is running on this connection"),
            observation.autocommit
        );
    }

    // Bound values reach both engines as the same bits.
    for value in [
        TaggedValue::Null,
        TaggedValue::Integer(-9223372036854775808),
        TaggedValue::Integer(9223372036854775807),
        TaggedValue::Real(1.5),
        TaggedValue::Real(-0.0),
        TaggedValue::Text(b"text".to_vec()),
        TaggedValue::Text(Vec::new()),
        TaggedValue::Blob(vec![0x00, 0xff]),
        TaggedValue::Blob(Vec::new()),
    ] {
        let observation = driver
            .send(&Op::Bind {
                sql: "SELECT ?1".to_string(),
                values: vec![value.clone()],
            })
            .expect("it runs");
        let mut statement = connection.prepare("SELECT ?1").expect("it prepares");
        statement
            .bind(1, tagged_to_datum(&value))
            .expect("it binds");
        assert!(statement.step().expect("it steps"));
        let ours = statement
            .row()
            .first()
            .map(tagged)
            .unwrap_or(TaggedValue::Null);
        let theirs = observation
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(TaggedValue::Null);
        assert!(ours.identical(&theirs), "{value:?}: {ours:?} vs {theirs:?}");
    }
    let _ = driver.send(&Op::Bye);
}

/// Converts a protocol value into an engine value.
fn tagged_to_datum(value: &TaggedValue) -> OwnedDatum {
    match value {
        TaggedValue::Null => OwnedDatum::Null,
        TaggedValue::Integer(integer) => OwnedDatum::Int(*integer),
        TaggedValue::Real(real) => OwnedDatum::Real(*real),
        TaggedValue::Text(text) => OwnedDatum::Text(text.clone()),
        TaggedValue::Blob(bytes) => OwnedDatum::Blob(bytes.clone()),
    }
}
