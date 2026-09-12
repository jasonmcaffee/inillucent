//! `inillucent import`, driven as the command it is rather than as a dot line.
//!
//! Invariant: **the verb loads the file it was given.** It did not, and nothing
//! here noticed for one release, because the only coverage `.import` had was a
//! shell script that passed no options. The verb always passes one - it builds
//! `.import --csv "file" "table"` from its `--format` parameter - and `.import`
//! read that first word as the file name, so every invocation of the verb died
//! with `cannot open "--csv"` while the dot command it delegates to stayed
//! green. A suite that exercises the layer underneath the broken one reports a
//! pass and means nothing; this one runs the command a person types.

use inillucent_cli::command::{self, Arguments, Context};
use inillucent_cli::json::Json;

/// Where this suite's scratch files live.
fn area() -> std::path::PathBuf {
    let path = inillucent_compat::workspace_root().join("_agent_output/cli-import");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Writes one scratch file and returns the path a command can be given.
///
/// @param name - the file's name inside the scratch area
/// @param content - what to write
fn written(name: &str, content: &str) -> String {
    let path = area().join(name);
    std::fs::write(&path, content).expect("the scratch file is written");
    path.to_string_lossy().replace('\\', "/")
}

/// Runs one command against a fresh in-memory database, with arguments.
///
/// @param context - the database the whole case runs against
/// @param name - the command to run
/// @param pairs - the parameters, as name and text value
fn run(
    context: &mut Context,
    name: &str,
    pairs: &[(&str, Json)],
) -> Result<command::Outcome, String> {
    let command = command::find(name).unwrap_or_else(|| panic!("{name} is in the command table"));
    let mut arguments = Arguments::default();
    for (key, value) in pairs {
        arguments.set(key, value.clone());
    }
    command::run(command, context, &arguments).map_err(|failure| failure.message)
}

/// Returns a text argument.
fn text(value: &str) -> Json {
    Json::Text(value.to_string())
}

/// Returns the one value a single-row single-column query produced.
///
/// @param context - the database to ask
/// @param sql - the query
fn scalar(context: &mut Context, sql: &str) -> String {
    let produced = run(context, "query", &[("sql", text(sql))]).expect("the query runs");
    produced
        .rows
        .first()
        .and_then(|row| row.first())
        .map(|value| match value {
            Json::Text(held) => held.clone(),
            Json::Int(number) => number.to_string(),
            Json::Real(number) => number.to_string(),
            Json::Null => String::new(),
            other => format!("{other:?}"),
        })
        .unwrap_or_default()
}

/// A CSV file loads, creating its table from the header row.
#[test]
fn the_verb_loads_a_csv_file() {
    let path = written(
        "people.csv",
        "id,name,note\n1,Seneca,\"a Stoic, and a playwright\"\n2,Epictetus,born a slave\n",
    );
    let mut context = Context::open(":memory:", false, None).expect("an in-memory database opens");
    let produced = run(
        &mut context,
        "import",
        &[("file", text(path.as_str())), ("table", text("people"))],
    )
    .expect("the import runs");
    assert_eq!(
        produced.changes, 2,
        "the import reported {} rows, and the file holds two",
        produced.changes
    );
    assert_eq!(scalar(&mut context, "SELECT count(*) FROM people"), "2");
    // The comma inside the quoted field is data, not a third column.
    assert_eq!(
        scalar(
            &mut context,
            "SELECT note FROM people WHERE name = 'Seneca'"
        ),
        "a Stoic, and a playwright"
    );
}

/// `--format tabs` reads a tab separated file.
#[test]
fn the_verb_reads_tab_separated_input() {
    let path = written("tabbed.tsv", "id\tname\n1\tZeno\n2\tChrysippus\n");
    let mut context = Context::open(":memory:", false, None).expect("an in-memory database opens");
    run(
        &mut context,
        "import",
        &[
            ("file", text(path.as_str())),
            ("table", text("stoics")),
            ("format", text("tabs")),
        ],
    )
    .expect("the import runs");
    assert_eq!(scalar(&mut context, "SELECT count(*) FROM stoics"), "2");
    assert_eq!(
        scalar(&mut context, "SELECT name FROM stoics WHERE id = '2'"),
        "Chrysippus"
    );
}

/// `--skip` drops leading rows, preamble and all.
#[test]
fn the_verb_skips_leading_rows() {
    let path = written(
        "preamble.csv",
        "# produced by something\nid,name\n1,Plato\n2,Aristotle\n",
    );
    let mut context = Context::open(":memory:", false, None).expect("an in-memory database opens");
    run(
        &mut context,
        "import",
        &[
            ("file", text(path.as_str())),
            ("table", text("philosophers")),
            // An integer, because that is the type the command table declares
            // for `skip` and what the command line parses `--skip 1` into. A
            // string here is silently ignored, which is its own small lesson.
            ("skip", Json::Int(1)),
        ],
    )
    .expect("the import runs");
    assert_eq!(
        scalar(&mut context, "SELECT count(*) FROM philosophers"),
        "2"
    );
    assert_eq!(
        scalar(&mut context, "SELECT name FROM philosophers WHERE id = '1'"),
        "Plato"
    );
}

/// A file that is not there is reported as a file that is not there.
///
/// The check that matters is the name in the message: the defect this suite was
/// written for reported `cannot open "--csv"`, which names a flag the caller
/// never typed and sends them looking for the wrong thing.
#[test]
fn a_missing_file_names_the_file() {
    let mut context = Context::open(":memory:", false, None).expect("an in-memory database opens");
    let outcome = run(
        &mut context,
        "import",
        &[
            ("file", text("no-such-file.csv")),
            ("table", text("nothing")),
        ],
    );
    let message = match outcome {
        Ok(produced) => produced.text,
        Err(message) => message,
    };
    assert!(
        message.contains("no-such-file.csv"),
        "the failure named neither the file nor anything useful: {message}"
    );
    assert!(
        !message.contains("--"),
        "the failure named an option the caller never typed: {message}"
    );
}

/// Loading a full-text table reports how many rows went in.
///
/// **It reported zero while loading 2,661 rows.** A virtual table's insert does
/// not move `total_changes`, which is what the verb counted, so the one number
/// the command prints said the opposite of what had happened - and the load is
/// the only way to fill an FTS5 table from a file, because
/// `INSERT INTO <virtual table> ... SELECT` is refused outright.
#[test]
fn loading_a_full_text_table_reports_its_rows() {
    let path = written(
        "fts.csv",
        "id,title,body\n1,Zeno,paradox of the arrow\n2,Zeno,Stoic founder\n",
    );
    let mut context = Context::open(":memory:", false, None).expect("an in-memory database opens");
    run(
        &mut context,
        "exec",
        &[(
            "sql",
            text("CREATE VIRTUAL TABLE notes USING fts5(id, title, body)"),
        )],
    )
    .expect("creates the full-text table");
    let produced = run(
        &mut context,
        "import",
        &[
            ("file", text(path.as_str())),
            ("table", text("notes")),
            ("skip", Json::Int(1)),
        ],
    )
    .expect("the import runs");
    assert_eq!(
        produced.changes, 2,
        "the import reported {} rows into a full-text table holding two",
        produced.changes
    );
    assert_eq!(scalar(&mut context, "SELECT count(*) FROM notes"), "2");
}
