//! What a schema is allowed to name, and what a defensive connection is
//! allowed to write.
//!
//! Invariant: a schema is data, and data does not get to choose what code runs.
//! `inillucent_ext::registry` has said that at the top of the file since it was
//! written; until task-1972 nothing enforced it, because
//! `Registry::authorize_function` had no caller anywhere in the workspace. A
//! `CHECK` constraint, an index expression, a generated column, a `DEFAULT`, a
//! partial-index predicate, a view and a trigger could each name any registered
//! function whatever its flags said, so `direct_only`, `innocuous` and
//! `PRAGMA trusted_schema` were a policy with a passing unit test and no effect
//! on the engine.
//!
//! These cases are the engine's answer rather than the registry's. The registry
//! is asked in `crates/inillucent-ext/src/registry.rs`'s own unit tests and it
//! answered correctly the whole time; what nothing asked was whether a
//! statement compiled against a schema that names such a function is refused.
//! Every case here goes through SQL, and each one **fails against the engine as
//! it was**.
//!
//! The function under test is registered by the test rather than being `embed`,
//! for two reasons. It needs no feature and no model, so these run in the
//! default build where `embed`'s own cases (`embed_direct_only.rs`) do not; and
//! the thing being checked is the mechanism, which a registration this file
//! controls can vary - `direct_only` against merely not innocuous, innocuous
//! against not, a trusted schema against an untrusted one - where one shipped
//! function can only ever exercise one corner of it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use inillucent_compat::facade::{Connection, Database};
use inillucent_ext::registry::FunctionFlags;
use inillucent_value::Value;

/// Returns a database file of this test's own, under the gitignored root.
///
/// A serial keeps two cases running in parallel off one another's file.
fn scratch() -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root().join("_agent_output/schema-function-policy");
    let _ = std::fs::create_dir_all(&root);
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = root.join(format!("{}-{serial}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Opens a database of this test's own, with one connection.
fn connect() -> Connection {
    let database = Database::open(scratch()).expect("opens");
    Box::leak(Box::new(database)).session().expect("connects")
}

/// Opens a connection holding `note(id, body)` and one registered function.
///
/// `risky` answers the length of its argument, so a `CHECK` or an index over it
/// is a schema that would work perfectly well if the policy let it - which is
/// what makes the refusals below refusals rather than errors about the
/// expression.
///
/// @param flags - what the registration promises about `risky`
fn connect_with(flags: FunctionFlags) -> Connection {
    let connection = connect();
    connection
        .create_scalar_function(
            "risky",
            1,
            flags,
            Arc::new(|arguments| {
                let length = match arguments.first() {
                    Some(Value::Text(text)) => text.utf8_bytes().len() as i64,
                    _ => 0,
                };
                Ok(Value::Integer(length))
            }),
        )
        .expect("registers");
    connection
        .execute("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")
        .expect("creates the table");
    connection
}

/// Returns the message a statement was refused with, failing if it succeeded.
///
/// @param connection - the connection to run it on
/// @param sql - the statement that should be refused
/// @param what - what the case is about, for the failure
fn refusal(connection: &Connection, sql: &str, what: &str) -> String {
    match connection.execute(sql) {
        Ok(_) => panic!("{what} was accepted: {sql}"),
        Err(error) => error.message().to_string(),
    }
}

/// Asserts a refusal names the function and says a schema may not call it.
///
/// The wording is `inillucent_sql::function::schema_refusal`'s, which
/// `Registry::authorize_function` reports too, so a message that drifts here
/// has drifted for an application asking the registry directly as well.
///
/// @param message - what the engine said
/// @param what - what the case is about, for the failure
fn says_top_level_only(message: &str, what: &str) {
    assert!(
        message.contains("risky") && message.contains("may only be used from top-level SQL"),
        "{what} was refused for the wrong reason: {message}"
    );
}

/// A statement may call a direct-only function; a `CHECK` may not.
///
/// The pair is the whole point. The first half is what stops the second from
/// being a test that a registration is unusable, and it is the half that has to
/// keep working: an application registers a function in order to call it.
#[test]
fn a_check_constraint_may_not_name_a_direct_only_function() {
    let connection = connect_with(FunctionFlags::external());
    assert_eq!(
        connection
            .query("SELECT risky('hello')")
            .expect("a statement may call it")
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(5)
    );
    connection
        .execute("CREATE TABLE guarded (b TEXT CHECK (risky(b) > 0))")
        .expect("the table is stored; the schema is not bound until it is used");
    let message = refusal(
        &connection,
        "INSERT INTO guarded (b) VALUES ('hello')",
        "a CHECK naming a direct-only function",
    );
    says_top_level_only(&message, "a CHECK naming a direct-only function");
}

/// An index on an expression naming a direct-only function is refused when the
/// index is created.
///
/// **This is the case the refusal had to reach before a row was read.** Such an
/// index is filled by a `SELECT` the engine builds out of the index's own
/// expression, and a `SELECT` is a statement - so before task-1972 the
/// expression ran with a statement's permissions once per row of the table, and
/// only the *next* write of the table was refused. The index was built, the
/// function had already run, and the table could no longer be written.
#[test]
fn an_index_expression_may_not_name_a_direct_only_function() {
    let connection = connect_with(FunctionFlags::external());
    connection
        .execute("INSERT INTO note (body) VALUES ('hello'), ('there')")
        .expect("two rows, so a build that read rows would run the function");
    let message = refusal(
        &connection,
        "CREATE INDEX note_risky ON note (risky(body))",
        "an index on an expression naming a direct-only function",
    );
    says_top_level_only(
        &message,
        "an index on an expression naming a direct-only function",
    );
}

/// A partial index whose predicate names a direct-only function is refused too.
#[test]
fn a_partial_index_predicate_may_not_name_a_direct_only_function() {
    let connection = connect_with(FunctionFlags::external());
    let message = refusal(
        &connection,
        "CREATE INDEX note_some ON note (body) WHERE risky(body) > 3",
        "a partial index whose predicate names a direct-only function",
    );
    says_top_level_only(
        &message,
        "a partial index whose predicate names a direct-only function",
    );
}

/// A generated column may not name a direct-only function.
#[test]
fn a_generated_column_may_not_name_a_direct_only_function() {
    let connection = connect_with(FunctionFlags::external());
    connection
        .execute("CREATE TABLE sized (b TEXT, n INTEGER GENERATED ALWAYS AS (risky(b)) STORED)")
        .expect("the table is stored");
    let message = refusal(
        &connection,
        "INSERT INTO sized (b) VALUES ('hello')",
        "a generated column naming a direct-only function",
    );
    says_top_level_only(&message, "a generated column naming a direct-only function");
}

/// A `DEFAULT` may not name a direct-only function.
#[test]
fn a_default_may_not_name_a_direct_only_function() {
    let connection = connect_with(FunctionFlags::external());
    connection
        .execute("CREATE TABLE defaulted (b TEXT, n INTEGER DEFAULT (risky('hello')))")
        .expect("the table is stored");
    let message = refusal(
        &connection,
        "INSERT INTO defaulted (b) VALUES ('x')",
        "a DEFAULT naming a direct-only function",
    );
    says_top_level_only(&message, "a DEFAULT naming a direct-only function");
}

/// A view's body may not name a direct-only function.
#[test]
fn a_view_may_not_name_a_direct_only_function() {
    let connection = connect_with(FunctionFlags::external());
    connection
        .execute("CREATE VIEW sizes AS SELECT risky(body) AS n FROM note")
        .expect("the view is stored");
    match connection.query("SELECT n FROM sizes") {
        Ok(_) => panic!("a view naming a direct-only function was read"),
        Err(error) => says_top_level_only(error.message(), "a view naming a direct-only function"),
    }
}

/// A trigger's body may not name a direct-only function.
#[test]
fn a_trigger_may_not_name_a_direct_only_function() {
    let connection = connect_with(FunctionFlags::external());
    connection
        .execute("CREATE TABLE audit (n INTEGER)")
        .expect("creates the audit table");
    connection
        .execute(
            "CREATE TRIGGER note_audit AFTER INSERT ON note \
             BEGIN INSERT INTO audit (n) VALUES (risky(NEW.body)); END",
        )
        .expect("the trigger is stored");
    let message = refusal(
        &connection,
        "INSERT INTO note (body) VALUES ('hello')",
        "a trigger naming a direct-only function",
    );
    says_top_level_only(&message, "a trigger naming a direct-only function");
}

/// A function that is not direct-only is admitted by a trusted schema and
/// refused by an untrusted one, and an innocuous one is admitted by both.
///
/// **The `innocuous` half of the policy, which the `direct_only` cases above
/// never reach.** `authorize_function` returns early for a trusted schema, so a
/// build that enforced only the direct-only bit would pass every case above and
/// fail this one - and `PRAGMA trusted_schema` would be a lever with nothing
/// behind it, which is what it was.
#[test]
fn trusted_schema_decides_whether_a_schema_may_name_a_function_that_is_not_innocuous() {
    let opaque = FunctionFlags {
        direct_only: false,
        innocuous: false,
        deterministic: true,
    };
    let connection = connect_with(opaque);
    connection
        .execute("CREATE TABLE guarded (b TEXT CHECK (risky(b) > 0))")
        .expect("the table is stored");
    connection
        .execute("INSERT INTO guarded (b) VALUES ('hello')")
        .expect("a trusted schema admits a function that is merely not innocuous");
    connection
        .execute("PRAGMA trusted_schema = OFF")
        .expect("the lever is settable");
    let message = refusal(
        &connection,
        "INSERT INTO guarded (b) VALUES ('there')",
        "a function that is not innocuous, named by an untrusted schema",
    );
    assert!(
        message.contains("risky") && message.contains("is not allowed in a schema"),
        "an untrusted schema refused for the wrong reason: {message}"
    );
}

/// An innocuous function is callable from a schema even when the schema is not
/// trusted.
///
/// This is what stops the case above from being a test that turning the lever
/// off refuses everything.
#[test]
fn an_untrusted_schema_still_admits_an_innocuous_function() {
    let connection = connect_with(FunctionFlags::builtin());
    connection
        .execute("PRAGMA trusted_schema = OFF")
        .expect("the lever is settable");
    connection
        .execute("CREATE TABLE guarded (b TEXT CHECK (risky(b) > 0))")
        .expect("the table is stored");
    connection
        .execute("INSERT INTO guarded (b) VALUES ('hello')")
        .expect("an innocuous function is what innocuous means");
}

/// `PRAGMA trusted_schema` reports the connection's own setting, on by default.
///
/// **It used to be a constant 0** from the fixed-answer table, beside the
/// comment "a schema object is never treated as trusted input here", while the
/// connection's policy said the opposite and nothing read either one. A
/// constant was harmless while the setting had no effect; it is a wrong answer
/// now that the binder consults it.
#[test]
fn trusted_schema_reports_the_setting_it_is_given() {
    let connection = connect();
    let read = |connection: &Connection| {
        connection
            .query("PRAGMA trusted_schema")
            .expect("answers")
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
    };
    assert_eq!(read(&connection), Some(1), "SQLite's default, and this one");
    connection
        .execute("PRAGMA trusted_schema = OFF")
        .expect("sets");
    assert_eq!(read(&connection), Some(0));
    connection
        .execute("PRAGMA trusted_schema = ON")
        .expect("sets");
    assert_eq!(read(&connection), Some(1));
}

/// A defensive connection refuses a write to a module's shadow table, and
/// allows a write to an ordinary table whose name merely starts the same way.
///
/// **`Registry::authorize_shadow_write` had the same defect
/// `authorize_function` had**: it was the whole of what `PRAGMA defensive`
/// promises about shadow tables, it read a flag nothing ever set, and nothing
/// called it. The shell turns defensive on for every connection it opens, so
/// the guarantee a person reading `.dbconfig` believed they had was refusing
/// exactly one thing, `PRAGMA journal_mode = OFF`.
///
/// The second half is the one that keeps the rule honest: `docs_backup` is an
/// ordinary table, and a check that decided what a shadow table is by looking
/// for an underscore after a virtual table's name would refuse it.
#[test]
fn a_defensive_connection_refuses_a_write_to_a_shadow_table() {
    let connection = connect();
    connection
        .execute("CREATE VIRTUAL TABLE docs USING fts5(body)")
        .expect("creates the index");
    connection
        .execute("CREATE TABLE docs_backup (body TEXT)")
        .expect("creates an ordinary table with a name that looks like a shadow");
    connection
        .execute("DELETE FROM docs_data WHERE 0")
        .expect("a shadow table is an ordinary table until the flag is on");
    connection.set_defensive(true).expect("turns defensive on");
    let message = refusal(
        &connection,
        "DELETE FROM docs_data WHERE 0",
        "a write to a shadow table on a defensive connection",
    );
    assert!(
        message.contains("docs_data") && message.contains("shadow table"),
        "a shadow write was refused for the wrong reason: {message}"
    );
    connection
        .execute("DELETE FROM docs_backup WHERE 0")
        .expect("an ordinary table is not a shadow table because of its name");
    connection
        .execute("INSERT INTO docs (body) VALUES ('hello')")
        .expect("and the module still writes its own storage, which it does not do through SQL");
}

/// `load_extension` refuses every path, which is why
/// `Registry::authorize_extension` having no caller is not the same defect.
///
/// There is nothing in this engine that loads a shared library: the SQL
/// function refuses, and so does the shell's `.load`. The policy is written
/// down for the day there is something to apply it to; what a caller has today
/// is this refusal, so this is what is checked.
#[test]
fn load_extension_refuses_every_path() {
    let connection = connect();
    match connection.query("SELECT load_extension('/tmp/anything.so')") {
        Ok(rows) => panic!("an extension load answered {rows:?}"),
        Err(error) => assert!(
            !error.message().is_empty(),
            "a refusal with no sentence in it"
        ),
    }
}
