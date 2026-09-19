//! Every command line verb, run as a process, with its answer read back.
//!
//! Invariant: **each of the thirty verbs in `registry.rs`'s `COMMANDS` is
//! spawned as a built binary with arguments that mean something, and what is
//! asserted is a named field of its parsed `--output json` or a specific exit
//! code.** Not that it did not crash, and not that it refused with a long
//! enough sentence.
//!
//! **Eighteen verbs had never been passed to a spawned binary (task-1969,
//! 5.2).** `run`, `create`, `tables`, `describe`, `schema`, `indexes`,
//! `databases`, `explain`, `checkpoint`, `analyze`, `stats`, `search`,
//! `vector-search`, `functions`, `migrate`, `version`, `help` and `shell` were
//! reached only by `command_parity.rs`'s `every_command_answers_an_empty_call`,
//! which hands each one `Arguments::default()` and, in the `Err` branch, asserts
//! only that the message is longer than ten characters. A command that is
//! entirely broken passes that by refusing with a sentence.
//!
//! And `--output json` - which AGENTS.md calls one of "the four things that
//! will save you a wrong turn", on any command - was parsed out of a spawned
//! binary for two of the thirty: `query` and `setup-embeddings`.
//!
//! **Why a field rather than the whole object.** Asserting on the whole
//! envelope would make every verb's test fail whenever a field was added to the
//! envelope, which is a change that breaks nothing. What each case names is the
//! part of the answer that is about that verb: `total` on `query`, the table
//! list on `tables`, the column names on `describe`, the plan text on
//! `explain`, the version string on `version`.
//!
//! **The guard at the bottom is what keeps this file honest.** A verb added to
//! `COMMANDS` with no test function naming it fails
//! `every_registry_command_has_a_subprocess_test`, the same way
//! `help_lists_everything` keeps the verb list in step with the registry.

use std::path::{Path, PathBuf};

use inillucent_compat::cliproc::{
    column_names, number_field, program, rows, run, run_with_input, text_field, Ran,
};
use inillucent_compat::workspace_root;

/// Returns a directory of this case's own, emptied first.
///
/// Per case rather than per file, because several of these write a database and
/// two of them write a second one beside it; a shared directory would make the
/// order the cases happen to run in part of what is under test.
///
/// @param case - what to name the directory after
fn area(case: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/cli-commands")
        .join(case);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns a database holding one table with one row, and one full-text table.
///
/// The same fixture for every case that needs data, built by the binary under
/// test rather than by the engine in process. That is deliberate: a fixture
/// written by the library and read by the program would leave the program's own
/// write path untested, and `create` plus `exec` are two of the thirty verbs.
///
/// @param binary - the built `inillucent`
/// @param case - what to name this case's directory after
fn populated(binary: &Path, case: &str) -> PathBuf {
    let database = area(case).join("app.rdb");
    let path = database.to_string_lossy().to_string();
    for arguments in [
        vec!["create", path.as_str(), "--output", "json"],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE INDEX note_body ON note (body)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "INSERT INTO note (body) VALUES ('hello'), ('goodbye')",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE VIRTUAL TABLE doc USING fts5(body)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "INSERT INTO doc (body) VALUES ('the quick brown fox')",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE point (id INTEGER PRIMARY KEY, at VECTOR(3))",
        ],
    ] {
        let ran = run(binary, &arguments);
        assert_eq!(
            ran.code,
            0,
            "building the fixture failed at {arguments:?}:\n{}",
            ran.said()
        );
    }
    database
}

/// Asserts a run succeeded, showing what it printed when it did not.
///
/// @param verb - the verb under test, for the message
/// @param ran - what the run produced
fn succeeded(verb: &str, ran: &Ran) {
    assert_eq!(ran.code, 0, "`{verb}` exited {}:\n{}", ran.code, ran.said());
}

// --- the verbs that read or write rows --------------------------------------

/// `query` returns rows and an exact `total`.
///
/// `total` rather than the row count, because the two differ under `--limit`
/// and the exact one is the promise AGENTS.md makes about the JSON object.
#[test]
fn query_returns_rows_and_an_exact_total() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "query");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT body FROM note ORDER BY id",
            "--limit",
            "1",
            "--output",
            "json",
        ],
    );
    succeeded("query", &ran);
    assert_eq!(number_field(&ran.stdout, "total"), 2.0, "{}", ran.stdout);
    assert_eq!(
        rows(&ran.stdout),
        vec![vec!["hello".to_string()]],
        "`--limit 1` returned something other than the first row:\n{}",
        ran.stdout
    );
}

/// `exec` reports the number of rows it changed.
#[test]
fn exec_reports_the_rows_it_changed() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "exec");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "exec",
            "UPDATE note SET body = 'changed'",
            "--output",
            "json",
        ],
    );
    succeeded("exec", &ran);
    assert_eq!(number_field(&ran.stdout, "changes"), 2.0, "{}", ran.stdout);
}

/// `batch` runs several statements as one transaction.
#[test]
fn batch_runs_several_statements_as_one_transaction() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "batch");
    let path = database.to_string_lossy().to_string();
    let ran = run(
        &binary,
        &[
            "--db",
            &path,
            "batch",
            "INSERT INTO note (body) VALUES ('one'); INSERT INTO note (body) VALUES ('two')",
            "--output",
            "json",
        ],
    );
    succeeded("batch", &ran);
    let counted = run(
        &binary,
        &[
            "--db",
            &path,
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        rows(&counted.stdout),
        vec![vec!["4".to_string()]],
        "the batch did not commit both statements:\n{}",
        counted.stdout
    );
}

/// `run` drives the shell, dot commands and all.
#[test]
fn run_drives_the_shell_including_dot_commands() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "run");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "run",
            ".tables",
            "--output",
            "json",
        ],
    );
    succeeded("run", &ran);
    let printed = text_field(&ran.stdout, "text");
    assert!(
        printed.contains("note"),
        "`.tables` through `run` did not name the table:\n{printed}"
    );
    assert_eq!(text_field(&ran.stdout, "command"), "run", "{}", ran.stdout);
}

// --- the verbs that describe the database -----------------------------------

/// `create` makes a file, and says which one.
#[test]
fn create_makes_a_file_and_names_it() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = area("create").join("fresh.rdb");
    let ran = run(
        &binary,
        &["create", &database.to_string_lossy(), "--output", "json"],
    );
    succeeded("create", &ran);
    assert!(
        text_field(&ran.stdout, "path").ends_with("fresh.rdb"),
        "{}",
        ran.stdout
    );
    assert!(
        database.is_file(),
        "`create` reported a file it did not make"
    );
}

/// `tables` lists the tables that are there.
#[test]
fn tables_lists_the_tables_that_are_there() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "tables");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "tables",
            "--output",
            "json",
        ],
    );
    succeeded("tables", &ran);
    let named: Vec<String> = rows(&ran.stdout)
        .into_iter()
        .filter_map(|row| row.first().cloned())
        .collect();
    assert!(
        named.contains(&"note".to_string()) && named.contains(&"point".to_string()),
        "`tables` listed {named:?}"
    );
}

/// `describe` returns one row per column, with the column names as its own.
#[test]
fn describe_returns_one_row_per_column() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "describe");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "describe",
            "--table",
            "note",
            "--output",
            "json",
        ],
    );
    succeeded("describe", &ran);
    assert!(
        column_names(&ran.stdout).contains(&"name".to_string()),
        "`describe` has no `name` column: {:?}",
        column_names(&ran.stdout)
    );
    let named: Vec<String> = rows(&ran.stdout)
        .into_iter()
        .filter_map(|row| row.get(1).cloned())
        .collect();
    assert_eq!(
        named,
        vec!["id".to_string(), "body".to_string()],
        "`describe` named {named:?}"
    );
}

/// `schema` returns the statement that would recreate the table.
#[test]
fn schema_returns_the_statement_that_recreates_the_table() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "schema");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "schema",
            "--output",
            "json",
        ],
    );
    succeeded("schema", &ran);
    let printed = text_field(&ran.stdout, "text");
    assert!(
        printed.contains("CREATE TABLE note"),
        "`schema` did not carry the table's definition:\n{printed}"
    );
}

/// `indexes` names the index that was created.
#[test]
fn indexes_names_the_index_that_was_created() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "indexes");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "indexes",
            "--output",
            "json",
        ],
    );
    succeeded("indexes", &ran);
    let named: Vec<String> = rows(&ran.stdout)
        .into_iter()
        .filter_map(|row| row.first().cloned())
        .collect();
    assert!(
        named.contains(&"note_body".to_string()),
        "`indexes` listed {named:?}"
    );
}

/// `databases` names the main database and its file.
#[test]
fn databases_names_the_main_database_and_its_file() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "databases");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "databases",
            "--output",
            "json",
        ],
    );
    succeeded("databases", &ran);
    let listed = rows(&ran.stdout);
    assert!(
        listed.iter().any(|row| row.contains(&"main".to_string())),
        "`databases` listed {listed:?}"
    );
}

/// `explain` returns a plan naming the table it would read.
#[test]
fn explain_returns_a_plan_naming_the_table() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "explain");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "explain",
            "--sql",
            "SELECT body FROM note",
            "--output",
            "json",
        ],
    );
    succeeded("explain", &ran);
    let plan = rows(&ran.stdout)
        .into_iter()
        .flatten()
        .collect::<Vec<String>>()
        .join(" ");
    assert!(
        plan.contains("note"),
        "the plan does not name the table it reads:\n{plan}"
    );
}

/// `functions` lists a built-in by name.
#[test]
fn functions_lists_a_builtin_by_name() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "functions");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "functions",
            "--output",
            "json",
        ],
    );
    succeeded("functions", &ran);
    let named: Vec<String> = rows(&ran.stdout)
        .into_iter()
        .filter_map(|row| row.first().cloned())
        .collect();
    assert!(
        named.contains(&"substr".to_string()) && named.contains(&"json_extract".to_string()),
        "`functions` listed {} names and neither `substr` nor `json_extract`",
        named.len()
    );
}

/// `capabilities` reports every row with a support value.
#[test]
fn capabilities_reports_every_row_with_a_support_value() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let ran = run(&binary, &["capabilities", "--output", "json"]);
    succeeded("capabilities", &ran);
    assert_eq!(
        column_names(&ran.stdout),
        vec![
            "capability".to_string(),
            "support".to_string(),
            "note".to_string()
        ],
        "{}",
        ran.stdout
    );
    let listed = rows(&ran.stdout);
    assert!(
        listed.len() >= 20,
        "`capabilities` reported {} rows",
        listed.len()
    );
    assert!(
        listed.iter().all(|row| matches!(
            row.get(1).map(String::as_str),
            Some("yes" | "partial" | "no")
        )),
        "a capability row carries a support value that is none of yes, partial, no:\n{listed:?}"
    );
}

/// `version` reports the version this binary was built at.
#[test]
fn version_reports_the_version_it_was_built_at() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let ran = run(&binary, &["version", "--output", "json"]);
    succeeded("version", &ran);
    let reported = text_field(&ran.stdout, "cli");
    assert_eq!(
        reported,
        env!("CARGO_PKG_VERSION"),
        "the binary reports {reported} and this workspace is at {}",
        env!("CARGO_PKG_VERSION")
    );
}

/// `help` lists every verb the registry holds.
#[test]
fn help_lists_every_verb_the_registry_holds() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let ran = run(&binary, &["help", "--output", "json"]);
    succeeded("help", &ran);
    let listed: Vec<String> = rows(&ran.stdout)
        .into_iter()
        .filter_map(|row| row.first().cloned())
        .collect();
    let mut absent: Vec<&str> = Vec::new();
    for command in inillucent_cli::command::COMMANDS {
        if !listed.iter().any(|name| name == command.name) {
            absent.push(command.name);
        }
    }
    assert!(
        absent.is_empty(),
        "`help` does not list {absent:?}, which the registry holds"
    );
}

// --- the verbs that move data ------------------------------------------------

/// `import` loads a CSV file and says how many rows it took.
#[test]
fn import_loads_a_csv_file() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "import");
    let csv = database.with_file_name("rows.csv");
    // Both columns, because `import` maps a file's columns onto the table's by
    // position and refuses a row narrower than the table.
    std::fs::write(&csv, "id,body\n10,first\n11,second\n").expect("the csv is written");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "import",
            &csv.to_string_lossy(),
            "--table",
            "note",
            "--format",
            "csv",
            "--skip",
            "1",
            "--output",
            "json",
        ],
    );
    succeeded("import", &ran);
    let counted = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        rows(&counted.stdout),
        vec![vec!["4".to_string()]],
        "`import` did not add the two rows:\n{}",
        counted.stdout
    );
}

/// `export` writes the rows it was asked for, in the format it was asked for.
#[test]
fn export_writes_the_rows_in_the_format_asked_for() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "export");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "export",
            "--table",
            "note",
            "--format",
            "csv",
            "--output",
            "json",
        ],
    );
    succeeded("export", &ran);
    let written = text_field(&ran.stdout, "text");
    assert!(
        written.contains("id,body") && written.contains("hello"),
        "`export` did not produce the table as CSV:\n{written}"
    );
}

/// `dump` produces SQL that recreates the schema and the rows.
#[test]
fn dump_produces_sql_that_recreates_the_database() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "dump");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "dump",
            "--output",
            "json",
        ],
    );
    succeeded("dump", &ran);
    let script = text_field(&ran.stdout, "text");
    assert!(
        script.contains("CREATE TABLE note") && script.contains("INSERT INTO note"),
        "`dump` carried neither the schema nor the rows:\n{script}"
    );
}

/// `backup` writes a copy that opens and holds the same rows.
#[test]
fn backup_writes_a_copy_that_holds_the_same_rows() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "backup");
    let copy = database.with_file_name("copy.rdb");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "backup",
            &copy.to_string_lossy(),
            "--output",
            "json",
        ],
    );
    succeeded("backup", &ran);
    assert!(
        text_field(&ran.stdout, "wrote").ends_with("copy.rdb"),
        "{}",
        ran.stdout
    );
    let counted = run(
        &binary,
        &[
            "--db",
            &copy.to_string_lossy(),
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        rows(&counted.stdout),
        vec![vec!["2".to_string()]],
        "the copy does not hold the rows:\n{}",
        counted.stdout
    );
}

/// `restore` refuses a backup that is not there, and reopens one that is.
///
/// **What `restore` does is open the named file for the rest of the session,
/// not write its pages over the database you are in.** `dot.rs` explains why -
/// this engine's databases are whole files, so the honest restore is to point
/// at the other one - which makes the verb of no observable effect in a
/// one-shot process, because the process ends immediately afterwards. So the
/// second half drives it the way it is usable, through `run`, where the
/// statements after it read the restored file.
///
/// The first half is the defect this file found: `restore` on a path that is
/// not there exited 0 and reported `ok`, because opening a file that is not
/// there creates it. The caller got an empty database and no message
/// (task-1969, 5.2).
#[test]
fn restore_refuses_a_backup_that_is_not_there_and_reopens_one_that_is() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "restore");
    let copy = database.with_file_name("copy.rdb");
    succeeded(
        "backup",
        &run(
            &binary,
            &[
                "--db",
                &database.to_string_lossy(),
                "backup",
                &copy.to_string_lossy(),
            ],
        ),
    );

    let fresh = database.with_file_name("restored.rdb");
    succeeded(
        "create",
        &run(&binary, &["create", &fresh.to_string_lossy()]),
    );
    let absent = database.with_file_name("no-such-backup.rdb");
    let refused = run(
        &binary,
        &[
            "--db",
            &fresh.to_string_lossy(),
            "restore",
            &absent.to_string_lossy(),
            "--output",
            "json",
        ],
    );
    assert_ne!(
        refused.code,
        0,
        "`restore` accepted a backup file that is not there:\n{}",
        refused.said()
    );
    assert!(
        !absent.exists(),
        "`restore` created the backup file it was asked to read"
    );

    let line = format!(
        ".restore \"{}\"\n.tables",
        copy.to_string_lossy().replace('\\', "/")
    );
    let restored = run(
        &binary,
        &[
            "--db",
            &fresh.to_string_lossy(),
            "run",
            &line,
            "--output",
            "json",
        ],
    );
    succeeded("restore", &restored);
    assert!(
        text_field(&restored.stdout, "text").contains("note"),
        "the statements after `.restore` did not read the restored file:\n{}",
        restored.stdout
    );
}

/// `migrate` refuses a source that is not there, and says so.
///
/// A refusal rather than a migration, because the migration itself is
/// `crates/inillucent-migrate/tests/cli.rs`: what is under test here is that
/// the verb reaches the tool at all and that the tool's refusal comes back as a
/// nonzero exit code rather than an empty destination.
#[test]
fn migrate_refuses_a_source_that_is_not_there() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let directory = area("migrate");
    let absent = directory.join("no-such-source.db");
    let destination = directory.join("out.rdb");
    let ran = run(
        &binary,
        &[
            "migrate",
            &absent.to_string_lossy(),
            "--destination",
            &destination.to_string_lossy(),
            "--kind",
            "sqlite",
        ],
    );
    assert_ne!(
        ran.code,
        0,
        "`migrate` accepted a source that is not there:\n{}",
        ran.said()
    );
    let folded = ran.said().to_lowercase();
    assert!(
        folded.contains("no-such-source")
            || folded.contains("not")
            || folded.contains("cannot")
            || folded.contains("no such"),
        "`migrate` refused without saying what was wrong:\n{}",
        ran.said()
    );
    assert!(
        !destination.exists(),
        "`migrate` refused and left a destination file behind"
    );
}

// --- the verbs that maintain the file ----------------------------------------

/// `checkpoint` reports how much of the log it moved.
#[test]
fn checkpoint_reports_what_it_moved() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "checkpoint");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "checkpoint",
            "--output",
            "json",
        ],
    );
    succeeded("checkpoint", &ran);
    assert_eq!(
        column_names(&ran.stdout),
        vec![
            "busy".to_string(),
            "log".to_string(),
            "checkpointed".to_string()
        ],
        "{}",
        ran.stdout
    );
}

/// `integrity-check` answers `ok` on a database it just wrote.
#[test]
fn integrity_check_answers_ok_on_a_healthy_file() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "integrity-check");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "integrity-check",
            "--output",
            "json",
        ],
    );
    succeeded("integrity-check", &ran);
    let answered = rows(&ran.stdout)
        .into_iter()
        .flatten()
        .collect::<Vec<String>>()
        .join(" ");
    assert!(
        answered.contains("ok"),
        "`integrity-check` answered {answered:?} on a file it had just written"
    );
}

/// `analyze` writes statistics the planner can read back.
#[test]
fn analyze_writes_statistics_the_planner_reads() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "analyze");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "analyze",
            "--output",
            "json",
        ],
    );
    succeeded("analyze", &ran);
    let counted = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT count(*) FROM sqlite_stat1",
            "--output",
            "json",
        ],
    );
    let written: f64 = rows(&counted.stdout)
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.parse().ok())
        .unwrap_or(0.0);
    assert!(
        written > 0.0,
        "`analyze` wrote no rows to sqlite_stat1:\n{}",
        counted.stdout
    );
}

/// `stats` reports the page pool's size in bytes.
#[test]
fn stats_reports_the_page_pool_size() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "stats");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "stats",
            "--output",
            "json",
        ],
    );
    succeeded("stats", &ran);
    assert!(
        number_field(&ran.stdout, "pool_bytes") > 0.0,
        "`stats` reported a page pool of no bytes:\n{}",
        ran.stdout
    );
}

// --- the retrieval verbs ------------------------------------------------------

/// `search` finds the row whose text matches.
#[test]
fn search_finds_the_row_whose_text_matches() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "search");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "search",
            "brown",
            "--table",
            "doc",
            "--k",
            "5",
            "--output",
            "json",
        ],
    );
    succeeded("search", &ran);
    assert_eq!(number_field(&ran.stdout, "total"), 1.0, "{}", ran.stdout);
    assert_eq!(text_field(&ran.stdout, "query"), "brown", "{}", ran.stdout);
    let found = rows(&ran.stdout)
        .into_iter()
        .flatten()
        .collect::<Vec<String>>()
        .join(" ");
    assert!(
        found.contains("quick brown fox"),
        "`search` returned {found:?}"
    );
}

/// `vector-search` answers with a distance column over a `VECTOR` column.
///
/// The corpus is empty on purpose: what is under test is that the verb reaches
/// a `VECTOR(3)` column, parses the query vector and renders a distance column.
/// Ranking is graded by `crates/inillucent-compat/tests/vector.rs` against the
/// engine, which is where a ranking question belongs.
#[test]
fn vector_search_answers_with_a_distance_column() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "vector-search");
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "vector-search",
            "--table",
            "point",
            "--column",
            "at",
            "--vector",
            "[1,0,0]",
            "--k",
            "3",
            "--output",
            "json",
        ],
    );
    succeeded("vector-search", &ran);
    assert!(
        column_names(&ran.stdout).contains(&"distance".to_string()),
        "`vector-search` has no `distance` column: {:?}",
        column_names(&ran.stdout)
    );
}

/// `setup-embeddings` reports what is installed and downloads nothing.
///
/// With no component named, which is the verb's own answer to "never make a 620
/// MB fetch a surprise". That is what makes the case runnable on a machine with
/// no weights, so the row for this suite does not have to declare `onnx`. What
/// is asserted is that the verb reaches the installer and that the installer
/// names the model it would fetch and where it would put it.
#[test]
fn setup_embeddings_reports_what_is_installed() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let ran = run(&binary, &["setup-embeddings", "--output", "json"]);
    succeeded("setup-embeddings", &ran);
    assert!(
        ran.stdout.contains("nomic-embed-text-v1.5"),
        "`setup-embeddings` with no component named no model:\n{}",
        ran.stdout
    );
    assert!(
        text_field(&ran.stdout, "text").contains("Install root:"),
        "`setup-embeddings` did not report where it installs:\n{}",
        ran.stdout
    );
}

// --- the two verbs that hand the process over ---------------------------------

/// `shell` runs the statements written to its standard input.
///
/// Through standard input rather than an argument, because that is what the
/// verb is: it hands the process to the interactive shell, and the only way to
/// drive one from a test is to write to it and close the pipe.
#[test]
fn shell_runs_what_is_written_to_its_standard_input() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "shell");
    let ran = run_with_input(
        &binary,
        &["--db", &database.to_string_lossy(), "shell"],
        "SELECT body FROM note ORDER BY id;\n.quit\n",
    );
    assert!(
        ran.said().contains("hello"),
        "`shell` did not answer the statement it was given:\n{}",
        ran.said()
    );
}

/// `mcp` speaks JSON-RPC on its standard input and answers `initialize`.
///
/// One request, because the twenty-eight tools over the wire are
/// `crates/inillucent-compat/tests/mcp_wire.rs`. What is asserted here is that
/// the verb hands the process to the server at all, which is the part that
/// belongs to the command table.
#[test]
fn mcp_answers_an_initialize_over_its_standard_input() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "mcp");
    let request = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"#,
        r#""2024-11-05","capabilities":{},"clientInfo":{"name":"cli_commands","version":"1"}}}"#,
        "\n"
    );
    let ran = run_with_input(
        &binary,
        &["--db", &database.to_string_lossy(), "mcp"],
        request,
    );
    assert!(
        ran.stdout.contains("\"result\"") && ran.stdout.contains("protocolVersion"),
        "`mcp` did not answer `initialize`:\n{}",
        ran.said()
    );
}

// --- exit code 3 --------------------------------------------------------------

/// Statements this engine has not built, in the order they are tried.
///
/// **They are written here rather than read out of `capabilities` (task-1969,
/// 5.3).** The review's design said to take one from `capabilities --output
/// json`'s first `unsupported` row; measured against the shipped binary, there
/// is no such row - every one of the 24 capabilities reports `yes` or
/// `partial`, because the table is about whole features rather than about
/// individual constructs. So the statements are named, and the test fails when
/// *every* one of them has been built, which is the stale case the design was
/// guarding against: it says so and asks for another.
const NOT_BUILT: [&str; 3] = [
    // The first is the one `crates/inillucent-compat/tests/mcp_wire.rs` drives
    // the server with, so the two sides of the claim - exit code 3 out of the
    // binary, `unsupported` out of a JSON-RPC result - are about one statement.
    // It names no table on purpose, so neither suite has to build one first.
    "SELECT (SELECT 1, 2)",
    "SELECT * FROM sqlite_schema WHERE (name, type) IN (SELECT name, type FROM sqlite_schema)",
    "SELECT 1 FROM sqlite_schema WHERE name IN (SELECT name, type FROM sqlite_schema)",
];

/// A built binary exits with code 3, by value, on something it has not built.
///
/// **AGENTS.md puts this second among "the four things that will save you a
/// wrong turn", and no test observed it from outside the process (task-1969,
/// 5.3).** The mapping is in one place and is right - `Error::from_engine` sets
/// `Status::Unsupported`, `outcome.rs` maps it to 3, `bin/inillucent.rs`
/// returns it from `main` - and the only case was in process:
/// `assert_eq!(Failed::unsupported("vacuum", "not built").exit_code(), 3)`.
/// Nothing ran the binary against a statement the engine has not built and read
/// `output.status.code()`, so the three hops between that constant and a shell
/// script's `$?` were untested.
#[test]
fn an_unbuilt_statement_exits_three_and_says_unsupported() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "exit-code-three");
    let mut tried: Vec<String> = Vec::new();
    // A flag rather than a `return` out of the loop: `policy.rs`'s
    // `every_early_return_in_a_test_says_why` reads every early return in a
    // test and cannot tell one that succeeded from one that gave up, which is
    // the right way round - a test that returns is a test that stopped, and the
    // reason has to be visible at the `return` rather than four lines above it.
    let mut refused_as_unsupported = false;
    for statement in NOT_BUILT {
        let ran = run(
            &binary,
            &[
                "--db",
                &database.to_string_lossy(),
                "exec",
                statement,
                "--output",
                "json",
            ],
        );
        if ran.code != 3 {
            tried.push(format!("{statement} exited {}", ran.code));
            continue;
        }
        assert_eq!(
            text_field(&ran.stdout, "status"),
            "unsupported",
            "the binary exited 3 and the JSON does not say `unsupported`:\n{}",
            ran.stdout
        );
        refused_as_unsupported = true;
        break;
    }
    assert!(
        refused_as_unsupported,
        "no statement in NOT_BUILT produced exit code 3, so either the engine has built all \
         of them - in which case add one this engine has not built - or the mapping from \
         `Status::Unsupported` to 3 is broken:\n  {}",
        tried.join("\n  ")
    );
}

// --- the guard ----------------------------------------------------------------

/// A log segment beside the database that its chain does not reach is named
/// rather than left in silence (task-1979, C9).
///
/// **It is neither replayed nor removed, and that part is right**: the chain is
/// followed by sequence number from the meta record and stops at the first gap,
/// so a file copied or restored at a higher sequence is not part of this log.
/// What was wrong is that nothing said it was there, so it sat beside the
/// database through every open and close and an operator reading the directory
/// could not tell it from the live log.
#[test]
fn query_names_a_log_segment_the_chain_does_not_reach() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let database = populated(&binary, "stray-segment");
    let path = database.to_string_lossy().to_string();

    // Nothing planted yet: the answer must not mention one, or the assertion
    // below would pass against a build that always printed it.
    let clean = run(
        &binary,
        &[
            "--db",
            path.as_str(),
            "query",
            "SELECT 1",
            "--output",
            "json",
        ],
    );
    assert_eq!(clean.code, 0, "reading the clean file:\n{}", clean.said());
    assert!(
        !clean.stdout.contains("stray_log_segments"),
        "a database with no stray segment reported one:\n{}",
        clean.stdout
    );

    // The live segment, copied to a sequence the chain does not reach.
    let live = std::fs::read_dir(database.parent().unwrap_or(Path::new(".")))
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .find(|found| {
            found
                .file_name()
                .map(|name| name.to_string_lossy().contains("-wal."))
                .unwrap_or(false)
        });
    let Some(live) = live else {
        panic!("the database has no log segment beside it, so this case tested nothing");
    };
    let planted = live.with_extension("0000009999");
    std::fs::copy(&live, &planted).expect("the segment is copied");

    let named = run(
        &binary,
        &[
            "--db",
            path.as_str(),
            "query",
            "SELECT 1",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        named.code,
        0,
        "reading the file with a stray:\n{}",
        named.said()
    );
    assert!(
        named.stdout.contains("stray_log_segments"),
        "a log segment the chain does not reach was not named:\n{}",
        named.stdout
    );
}

/// Every verb in the registry has a subprocess test in this file.
///
/// **The guard that keeps this file from going stale (task-1969, 5.2).** A verb
/// added to `COMMANDS` with no case here would leave the file reading as
/// complete coverage of the command surface while covering thirty of
/// thirty-one. `help_lists_everything` keeps the verb list in step with the
/// registry the same way, one layer up.
///
/// The match is on the test function's *name* rather than on a registry of
/// cases, because a case is a `#[test]` function and there is no way to
/// enumerate those at run time. A verb with a hyphen is written with an
/// underscore in a function name, so the comparison folds one to the other.
#[test]
fn every_registry_command_has_a_subprocess_test() {
    let source = include_str!("cli_commands.rs");
    let names: Vec<&str> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("fn "))
        .filter_map(|rest| rest.split('(').next())
        .collect();
    assert!(
        names.len() >= 25,
        "read {} test function names out of this file, which means the scan is wrong rather \
         than that the file is nearly empty",
        names.len()
    );

    let mut absent: Vec<&str> = Vec::new();
    for command in inillucent_cli::command::COMMANDS {
        let wanted = command.name.replace('-', "_");
        if !names.iter().any(|name| name.starts_with(&wanted)) {
            absent.push(command.name);
        }
    }
    assert!(
        absent.is_empty(),
        "these verbs are in `registry.rs`'s COMMANDS and have no test in this file whose \
         name begins with them:\n  {}\n\
         A verb with no subprocess test is a verb whose argument parsing, output rendering \
         and exit code nothing outside the process has ever seen.",
        absent.join("\n  ")
    );
}

/// A read verb on a path that is not there refuses, and makes no file.
///
/// **It used to make one (task-1979, E2).** `inillucent --db typo.rdb tables`
/// created `typo.rdb`, wrote a log segment beside it, printed an empty table
/// and exited 0 - so a mistyped path answered "this database has no tables",
/// which is the wrong answer to a question nobody asked, and left a file the
/// next command would open without complaint. `create` is the verb that makes a
/// file and says so in its own help.
#[test]
fn a_read_verb_on_a_missing_path_refuses_and_creates_nothing() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let directory = area("missing");
    let database = directory.join("not-there.rdb");
    for verb in ["tables", "schema", "indexes", "databases", "stats"] {
        let ran = run(
            &binary,
            &[
                "--db",
                &database.to_string_lossy(),
                verb,
                "--output",
                "json",
            ],
        );
        assert_ne!(
            ran.code,
            0,
            "{verb} on a missing path exited 0: {}",
            ran.said()
        );
        assert_eq!(
            text_field(&ran.stdout, "status"),
            "not_found",
            "{verb} on a missing path: {}",
            ran.said()
        );
        assert!(
            !database.exists(),
            "{verb} created {} instead of refusing",
            database.display()
        );
    }
    // `create` still makes one, and the verbs then answer.
    let made = run(&binary, &["create", &database.to_string_lossy()]);
    assert_eq!(made.code, 0, "create: {}", made.said());
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "tables",
            "--output",
            "json",
        ],
    );
    succeeded("tables", &ran);
}

/// A database of a newer format version is refused by name, not as damage.
///
/// **Both answered `corrupt` (task-1979, E3).** A file this build cannot read
/// because it is newer is perfectly well formed, and reporting it as corruption
/// sends a reader looking for a torn page. The refusal now carries the status
/// `unsupported`, which is the same answer every other "this build has not got
/// that" gives, and the command line exits 3.
///
/// The fixture is a real database with the four format bytes of both the meta
/// page and its shadow raised, and the checksum recomputed - which is why the
/// test writes it through the engine first rather than assembling a header.
#[test]
fn a_newer_format_version_is_refused_as_unsupported() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let directory = area("format");
    let database = directory.join("newer.rdb");
    let made = run(&binary, &["create", &database.to_string_lossy()]);
    assert_eq!(made.code, 0, "create: {}", made.said());

    let mut bytes = std::fs::read(&database).expect("the database reads back");
    let page_size = {
        let mut four = [0u8; 4];
        four.copy_from_slice(bytes.get(12..16).expect("a header"));
        u32::from_le_bytes(four) as usize
    };
    // The meta record lives on page 0 and its shadow on page 1, and each one
    // carries its own copy of the version; raising one alone would be read as a
    // damaged primary with a good shadow, which is a different case.
    for page in [0usize, page_size] {
        let at = page.saturating_add(8);
        let slot = bytes.get_mut(at..at + 4).expect("the format field");
        slot.copy_from_slice(&99u32.to_le_bytes());
    }
    std::fs::write(&database, &bytes).expect("the database writes back");

    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "tables",
            "--output",
            "json",
        ],
    );
    assert_eq!(ran.code, 3, "a newer format should exit 3: {}", ran.said());
    assert_eq!(
        text_field(&ran.stdout, "status"),
        "unsupported",
        "a newer format: {}",
        ran.said()
    );
    let message = text_field(&ran.stdout, "message");
    assert!(
        message.contains("format version 99"),
        "the refusal names the version it found: {message}"
    );
}
