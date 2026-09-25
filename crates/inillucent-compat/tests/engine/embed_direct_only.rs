//! `embed(TEXT)` is a function a schema may not name, at the level SQL sees.
//!
//! Invariant: a schema is data, and data does not get to choose what code runs.
//! `embed` loads a 275 MB ONNX model, so a `CHECK` constraint that names it
//! loads that model on every insert and an index expression that names it loads
//! it once per row of the table, inside the `CREATE INDEX` itself.
//!
//! **The flag and the enforcement are two different things, and they were fixed
//! by two different tickets.** task-1970 corrected the registration: `embed`
//! had been registered `FunctionFlags { deterministic: true, ..default() }`, and
//! the `Default` derive is every flag false, so the flag said a schema may name
//! it while the function's own doc comment said "It stays `direct_only`".
//! `crates/inillucent-search/src/embed.rs` asserts the flag and asks the
//! registry directly, and both answered correctly the moment the registration
//! was fixed. What nothing asked was the engine, because
//! `Registry::authorize_function` had no caller - so every case below was
//! **accepted** by a build whose unit tests were green. task-1972 gave the
//! binder a call site and these are its SQL-level cases.
//!
//! **No model, and no `onnx`.** Every case here is refused while the statement
//! is being bound, so nothing ever calls `embed` and nothing loads weights. The
//! feature is needed only because it is what registers the name: without
//! `inillucent-engine/embed` there is no `embed` to refuse, and a file that
//! quietly tested "an unknown function is an unknown function" would be the
//! green-that-checked-nothing this repository counts.

use std::sync::atomic::{AtomicUsize, Ordering};

use inillucent_compat::facade::{Connection, Database};

/// Returns a database file of this test's own, under the gitignored root.
fn scratch() -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root().join("_agent_output/embed-direct-only");
    let _ = std::fs::create_dir_all(&root);
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = root.join(format!("{}-{serial}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Opens a connection holding an empty `note(id, body)`.
///
/// Empty on purpose: an index build reads the table, and a case that slipped
/// through on a populated one would try to load the model rather than report
/// that it was accepted.
fn connect() -> Connection {
    let database = Database::open(scratch()).expect("opens");
    let connection = Box::leak(Box::new(database)).session().expect("connects");
    connection
        .execute("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")
        .expect("creates the table");
    connection
}

/// Whether this build registered `embed` at all, said once per process.
///
/// **`coverage.mjs` is why this exists** (task-2066). It runs
/// `cargo test --tests --release --workspace` with no features, because
/// `inillucent-engine/embed` reaches `inillucent-core/onnx` and the coverage run
/// excludes `inillucent-core` on purpose - a machine with no model measures it
/// as zero. So every case in this file ran without the function they are about
/// and failed, which took the exit code of the whole documented coverage command
/// with it. It had been doing that since this file arrived on 2026-09-17 and was
/// found six days later by the first person to run the command.
///
/// **A skip is not the green-that-checked-nothing this file's header warns
/// about.** That warning is about a file quietly testing "an unknown function is
/// an unknown function" and reporting success. This says out loud that it did
/// not run, `--strict` turns it into a failure, and the target's `features` row
/// in `tests/selection.toml` means the runner always builds it with `embed` -
/// so the only build that skips is one that deliberately left the feature out.
///
/// **The probe prepares rather than runs**, which is the same thing
/// `a_statement_may_name_embed` relies on and for the same reason: binding is
/// where the policy is applied, and running is where the 275 MB model would be
/// loaded. A build with the feature and no model on disk prepares this exactly
/// as one with the model does.
///
/// A `CHECK` constraint is the wrong probe and was tried first: this engine
/// stores a schema expression as text and binds it on the write, so
/// `CREATE TABLE ... CHECK (embed(...))` is *accepted* and says nothing about
/// whether the function exists.
fn embed_is_registered() -> bool {
    let connection = connect();
    match connection.prepare("SELECT embed('hello')") {
        Ok(_) => true,
        Err(error) => !error.message().contains("no embedding support compiled in"),
    }
}

/// Opens a connection, or says why these cases did not run.
///
/// See [`embed_is_registered`].
fn connect_or_skip() -> Option<Connection> {
    if !embed_is_registered() {
        inillucent_compat::differential::skipping(
            "this build has no embedding support compiled in, so there is no `embed` to \
             refuse; build with `inillucent-engine/embed`, which `tests/selection.toml` \
             declares for this target",
        );
        return None;
    }
    Some(connect())
}

/// Asserts a statement is refused for naming `embed` from a schema.
///
/// @param connection - the connection to run it on
/// @param sql - the statement that should be refused
/// @param what - what the case is about, for the failure
fn refused_as_schema(connection: &Connection, sql: &str, what: &str) {
    match connection.execute(sql) {
        Ok(_) => panic!("{what} was accepted: {sql}"),
        Err(error) => assert!(
            error.message().contains("embed")
                && error
                    .message()
                    .contains("may only be used from top-level SQL"),
            "{what} was refused for the wrong reason: {}",
            error.message()
        ),
    }
}

/// An index on `embed(body)` is refused when the index is created.
///
/// This is the case the function's own doc comment names: "A
/// `CREATE INDEX i ON t (embed(body))` would then load the model once per row of
/// the table, inside the statement that creates the index."
#[test]
fn an_index_expression_may_not_name_embed() {
    let Some(connection) = connect_or_skip() else {
        return;
    };
    refused_as_schema(
        &connection,
        "CREATE INDEX note_vec ON note (embed(body))",
        "an index on embed(body)",
    );
}

/// A `CHECK` constraint may not name `embed`.
///
/// The `CREATE TABLE` is stored: a schema object's expressions are text in this
/// engine until something binds them, so the refusal lands on the write, which
/// is the statement that would have loaded the model.
#[test]
fn a_check_constraint_may_not_name_embed() {
    let Some(connection) = connect_or_skip() else {
        return;
    };
    connection
        .execute("CREATE TABLE guarded (b TEXT CHECK (length(embed(b)) > 0))")
        .expect("the table is stored");
    refused_as_schema(
        &connection,
        "INSERT INTO guarded (b) VALUES ('hello')",
        "a CHECK naming embed",
    );
}

/// A generated column may not name `embed`.
#[test]
fn a_generated_column_may_not_name_embed() {
    let Some(connection) = connect_or_skip() else {
        return;
    };
    connection
        .execute("CREATE TABLE doc (b TEXT, v BLOB GENERATED ALWAYS AS (embed(b)) STORED)")
        .expect("the table is stored");
    refused_as_schema(
        &connection,
        "INSERT INTO doc (b) VALUES ('hello')",
        "a generated column naming embed",
    );
}

/// A statement may still name `embed`, which is what makes the refusals above
/// a policy rather than a removal.
///
/// It is prepared and not run: binding is where the policy is applied, and
/// running it is where the 275 MB model would be loaded. A build with the
/// feature but no model on disk compiles this statement exactly as one with the
/// model does.
#[test]
fn a_statement_may_name_embed() {
    let Some(connection) = connect_or_skip() else {
        return;
    };
    connection
        .prepare("SELECT embed('hello')")
        .expect("a statement may call embed");
    connection
        .prepare("SELECT embed(body) FROM note")
        .expect("and may call it over a column");
}

/// A trusted schema still may not name `embed`.
///
/// `PRAGMA trusted_schema` is on by default, here and in SQLite, and
/// `direct_only` is the flag that beats it: the rule returns early for a
/// trusted schema *unless* the function is direct-only. A build that enforced
/// only the `innocuous` half would let `embed` through on every machine that
/// had not turned the pragma off, which is every machine.
#[test]
fn a_trusted_schema_may_not_name_embed() {
    let Some(connection) = connect_or_skip() else {
        return;
    };
    let trusted = connection
        .query("PRAGMA trusted_schema")
        .expect("answers")
        .first()
        .and_then(|row| row.first())
        .and_then(inillucent_value::Value::as_integer);
    assert_eq!(
        trusted,
        Some(1),
        "this case is about the lever being on, and it is off"
    );
    connection
        .execute("CREATE TABLE guarded (b TEXT CHECK (length(embed(b)) > 0))")
        .expect("the table is stored");
    refused_as_schema(
        &connection,
        "INSERT INTO guarded (b) VALUES ('hello')",
        "a CHECK naming embed on a trusted schema",
    );
}
