//! Every statement Nikaya prepares, prepared and run here, at every arm.
//!
//! Invariant: **a statement the consumer's source contains is a statement this
//! engine accepts, or its refusal is written down in `allow.list` with a ticket
//! key.** Nothing about the rows: what this asserts is that the statement
//! compiles and runs, because that is the failure the escape it is shaped
//! around was.
//!
//! ## The escape
//!
//! `2f820f3`. A compound `SELECT` used as a derived table was refused by the
//! binder, and Nikaya's document view answered HTTP 500. No data would have
//! found it - the statement never compiled - and no per-construct test did
//! either, because a compound `SELECT` works and a derived table works and
//! nobody had written one inside the other. What was missing was the
//! consumer's own statement.
//!
//! So the corpus is the consumer's. `tests/workloads/nikaya/statements.sql`
//! holds every SQL literal in `C:/jason/dev/nikaya/server/src`, extracted by
//! `tools/extract-nikaya-workload.py`, with the schema its four migrations
//! build and one parameter value per placeholder. Nikaya's data is private mail
//! and none of it is here.
//!
//! ## What an allow list entry means
//!
//! The same thing it means in `differential_part8.rs`: this statement does not
//! work yet, here is the ticket, and **a statement that starts working fails
//! this test until its entry is removed.** A list that only grows is a list of
//! things nobody will ever take off it.
//!
//! ## Why it runs at every arm
//!
//! Because a statement that compiles is not a statement that runs: a `SELECT`
//! over a table whose rows are wider than a 4,096 byte page takes a different
//! read path from the same `SELECT` at 32,768, and task-2033 is what that
//! difference costs when nothing exercises it.

use std::collections::BTreeMap;
use std::path::Path;

use inillucent_compat::matrix::Arm;
use inillucent_compat::scenario;
use inillucent_compat::stories::{open, reopen_and_check, run, Params};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::Connection;
use inillucent_tree::datum::OwnedDatum;

/// One statement out of the workload file.
struct Statement {
    /// Where it is in Nikaya's source, which is also its allow list key.
    source: String,
    /// The SQL.
    sql: String,
    /// One value per placeholder.
    params: Vec<OwnedDatum>,
}

/// Reads the workload file: the schema, and the statements with their values.
///
/// The format is two `-- section:` markers and then one block per statement,
/// described in the file's own header. A parse that finds nothing is a failure
/// rather than an empty run, because an empty corpus passes every assertion
/// below (rule 1.2).
fn workload() -> (String, Vec<Statement>) {
    let path = workspace_root().join("tests/workloads/nikaya/statements.sql");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()));

    // **A marker is a whole line, and this is the second version of that.** The
    // first split on the text `-- section: schema` anywhere, and the file's own
    // header describes the format - so the split landed inside the sentence
    // explaining the marker and handed the schema builder a paragraph of prose,
    // which the engine then reported as a syntax error near a backtick.
    let schema: String = text
        .lines()
        .skip_while(|line| line.trim() != "-- section: schema")
        .take_while(|line| line.trim() != "-- section: statements")
        .skip(1)
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        schema.contains("CREATE TABLE"),
        "{} has no `-- section: schema` line, or nothing under it",
        path.display()
    );
    let statements_part: String = text
        .lines()
        .skip_while(|line| line.trim() != "-- section: statements")
        .skip(1)
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        statements_part.contains("-- statement:"),
        "{} has no `-- section: statements` line, or nothing under it",
        path.display()
    );

    let mut statements: Vec<Statement> = Vec::new();
    let mut source = String::new();
    let mut params: Vec<OwnedDatum> = Vec::new();
    let mut sql = String::new();
    for line in statements_part.lines() {
        if let Some(rest) = line.strip_prefix("-- statement: ") {
            if !sql.trim().is_empty() {
                statements.push(Statement {
                    source: std::mem::take(&mut source),
                    sql: std::mem::take(&mut sql).trim().to_string(),
                    params: std::mem::take(&mut params),
                });
            }
            source = rest.trim().to_string();
            sql.clear();
            params.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("-- params: ") {
            params = parse_params(rest.trim(), &source);
            continue;
        }
        if line.starts_with("-- ") {
            continue;
        }
        sql.push_str(line);
        sql.push('\n');
    }
    if !sql.trim().is_empty() {
        statements.push(Statement {
            source,
            sql: sql.trim().to_string(),
            params,
        });
    }

    assert!(
        statements.len() >= 100,
        "read {} statements out of {}, which means the format is being parsed wrongly rather \
         than that Nikaya has almost no SQL in it",
        statements.len(),
        path.display()
    );
    (schema, statements)
}

/// Reads a `-- params:` line, which is a JSON array of strings and numbers.
///
/// Hand written rather than taken from a JSON reader, because the whole of what
/// is in these arrays is a quoted string or an integer, and the values are
/// written by `tools/extract-nikaya-workload.py` rather than by a person.
///
/// **It splits on the commas between values, not on every comma.** Nikaya binds
/// an embedding as a string that is itself a bracketed list -
/// `"[0.1,0.2,0.3,0.4]"` is one parameter of
/// `repositories/chunks.rs:115` - and a split on every comma turned that one
/// value into four fragments, the first of them `"[0.1`, which is neither a
/// string nor an integer. The parameter a consumer actually binds is the thing
/// this file exists to replay, so the reader has to keep it whole.
///
/// @param text - the array, as it appears in the file
/// @param source - which statement it belongs to, for the failure message
fn parse_params(text: &str, source: &str) -> Vec<OwnedDatum> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{source}: `{text}` is not a JSON array"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    split_values(inner)
        .iter()
        .map(|item| {
            let item = item.trim();
            match item
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
            {
                Some(text) => OwnedDatum::Text(text.as_bytes().to_vec()),
                None => match item.parse::<i64>() {
                    Ok(number) => OwnedDatum::Int(number),
                    Err(_) => panic!("{source}: `{item}` is neither a string nor an integer"),
                },
            }
        })
        .collect()
}

/// Splits an array's body on the commas that separate its values.
///
/// A comma inside a quoted string belongs to the string. There is no escaping
/// to handle: `tools/extract-nikaya-workload.py` writes the file and refuses a
/// value holding a quote, so a `"` here always opens or closes one.
///
/// @param inner - the text between the array's brackets
fn split_values(inner: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in inner.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            ',' if !quoted => {
                values.push(current.clone());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    values.push(current);
    values
}

/// Reads the allow list: the statements that do not work yet, and their ticket.
fn allowed() -> BTreeMap<String, String> {
    let path = workspace_root().join("tests/workloads/nikaya/allow.list");
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((source, said)) = line.split_once('\t') else {
            panic!("{}: an allow line has no tab: {line}", path.display());
        };
        out.insert(source.trim().to_string(), said.trim().to_string());
    }
    out
}

/// Binds a statement's values into the engine's parameter set.
///
/// @param values - the values from the workload file
fn bind(values: &[OwnedDatum]) -> Params {
    let mut params = Params::new();
    for (index, value) in values.iter().enumerate() {
        let slot = u32::try_from(index + 1).unwrap_or(u32::MAX);
        params.set(slot, value.clone());
    }
    params
}

/// Writes a handful of rows into every table the schema declares.
///
/// The values match what `tools/extract-nikaya-workload.py` binds - `row-0001`
/// for a text column and `1` for an integer one - so a statement whose
/// predicate is `= ?1` matches a row rather than nothing. A statement that
/// matched nothing would still be a real test of the binder and the planner,
/// which is what this suite is about, but it would be a weaker test of the read
/// path and it would not exercise a large value at all.
///
/// @param connection - the connection to write through
fn seed(connection: &Connection<'_>) {
    for table in TABLES {
        run(connection, table);
    }
}

/// The rows every statement runs against.
///
/// Written out rather than generated, because the columns they have to satisfy
/// are Nikaya's `NOT NULL`s, `CHECK`s and foreign keys: a generator would be a
/// second copy of the schema, kept in step by hand.
const TABLES: &[&str] = &[
    "INSERT INTO source_account VALUES ('row-0001', 'gmail', 'row-0001', 'row-0001', \
     'authorized', 1, 1);",
    "INSERT INTO gmail_sync_state VALUES ('row-0001', 1, 'row-0001', 1, 1, 1, 1, 1);",
    "INSERT INTO document VALUES ('row-0001', 'row-0001', 'email', 'row-0001', 'row-0001', \
     'row-0001', 'row-0001', NULL, 'row-0001', 1, NULL, 1, 1, 1);",
    "INSERT INTO email_message VALUES ('row-0001', 'row-0001', 'row-0001', 'row-0001', \
     'row-0001', 'row-0001', 1, '[]', 'row-0001', 1, 1, 0);",
    "INSERT INTO email_participant (document_id, kind, display_name, address, ordinal) \
     VALUES ('row-0001', 'from', 'row-0001', 'row-0001', 1);",
    "INSERT INTO attachment VALUES ('row-0001', 'row-0001', 'row-0001', 1, 'row-0001', \
     'row-0001', 'extracted', 'row-0001', 'row-0001', NULL, 1, 1, '{}');",
    "INSERT INTO message_attachment (document_id, attachment_id, gmail_part_id, \
     gmail_attachment_id, content_id, original_filename, is_inline, ordinal) \
     VALUES ('row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', 0, 1);",
    "INSERT INTO attachment_download_queue (document_id, gmail_message_id, gmail_part_id, \
     gmail_attachment_id, original_filename, mime_type, content_id, is_inline, declared_size, \
     ordinal, status, attempts, error_summary, created_at) \
     VALUES ('row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', \
     'row-0001', 0, 1, 1, 'queued', 0, NULL, 1);",
    "INSERT INTO chunk VALUES ('row-0001', 'row-0001', 1, 'row-0001', 'row-0001', 'row-0001', \
     1, 'row-0001', 'row-0001', 1, 0, NULL, NULL);",
    "INSERT INTO chunk_embedding VALUES ('row-0001', 'row-0001', 4, '[0.1,0.2,0.3,0.4]', 1);",
    "INSERT INTO sync_job (source_account_id, job_type, status, counters, checkpoint, attempts, \
     error_summary, started_at, finished_at, created_at, updated_at) \
     VALUES ('row-0001', 'row-0001', 'queued', '{}', 'row-0001', 0, NULL, 1, 1, 1, 1);",
    "INSERT INTO document_meta VALUES ('row-0001', 'email', 'row-0001', 'row-0001', '[]', \
     'row-0001', 0, 1);",
    "INSERT INTO app_user VALUES ('row-0001', 'row-0001', 'owner', 'row-0001', 1, 1);",
    "INSERT INTO app_session VALUES ('row-0001', 'row-0001', 1, 1, 1);",
    "INSERT INTO agent_session VALUES ('row-0001', 'row-0001', 1, 1);",
    "INSERT INTO embedding_queue VALUES ('row-0001', 1);",
    "INSERT INTO schema_migration VALUES ('row-0001', 1);",
];

/// Every statement Nikaya prepares is one this engine accepts.
///
/// The `chunk_embedding.embedding` column is `VECTOR(768)` in Nikaya and
/// `VECTOR(4)` here, because the width is a property of the model rather than
/// of the statement and 768 floats a row is a fixture rather than a seed. Every
/// statement that names the column names it by name, so the width does not
/// reach the SQL.
fn every_statement_the_consumer_prepares_runs(arm: &Arm, area: &Path) {
    let path = area.join("workload.rdb");
    let (schema, statements) = workload();
    let allow = allowed();

    let database = open(arm, &path);
    let connection = database.session();
    // `VECTOR(768)` is Nikaya's; four is enough for the column to be the type
    // it is, and the seed rows carry a four wide value.
    run(&connection, &schema.replace("VECTOR(768)", "VECTOR(4)"));
    seed(&connection);

    // **Two questions, and only the first one is about the engine.**
    //
    // *Does it compile?* That is what `2f820f3` failed: the binder declined a
    // compound `SELECT` used as a derived table, and no value would have
    // changed the answer. A refusal here is a failure.
    //
    // *Does it run?* The values are generated, not Nikaya's, so a statement can
    // be perfectly good and still be refused because `row-0001` is already in
    // the table it is being inserted into, or is not one of the five strings a
    // `CHECK` allows. Those refusals are about the seed and the schema, and
    // counting them as failures would mean writing a row per statement that
    // satisfies every constraint Nikaya has - which is Nikaya's fixtures, which
    // is its private mail. So a `Constraint` refusal is an outcome and anything
    // else is a failure.
    let mut compiled = 0usize;
    let mut answered = 0usize;
    let mut declined_by_a_constraint = 0usize;
    let mut refused: Vec<String> = Vec::new();
    let mut working_after_all: Vec<String> = Vec::new();
    for statement in &statements {
        let listed = allow.contains_key(&statement.source);
        let complaint = match connection.prepare(&statement.sql) {
            Ok(_) => {
                compiled += 1;
                let params = bind(&statement.params);
                match connection.query_with(&statement.sql, &params) {
                    Ok(_) => {
                        answered += 1;
                        None
                    }
                    Err(why) if why.code() == inillucent_base::PrimaryCode::Constraint => {
                        declined_by_a_constraint += 1;
                        None
                    }
                    Err(why) => Some(format!("running it: {} ({:?})", why.message(), why.code())),
                }
            }
            Err(why) => Some(format!(
                "preparing it: {} ({:?})",
                why.message(),
                why.code()
            )),
        };
        match (complaint, listed) {
            (None, true) => working_after_all.push(statement.source.clone()),
            (None, false) => {}
            (Some(_), true) => {}
            (Some(said), false) => refused.push(format!(
                "{}: {said}\n    {}",
                statement.source,
                statement.sql.replace('\n', "\n    ")
            )),
        }
    }

    assert!(
        refused.is_empty(),
        "these statements are in Nikaya's source and this engine will not take them at the {} \
         arm. A consumer's statement that does not compile is an HTTP 500 - `2f820f3` was \
         exactly this. Fix it, or add its source to tests/workloads/nikaya/allow.list with the \
         ticket that will:\n  {}",
        arm.name,
        refused.join("\n  ")
    );
    assert!(
        working_after_all.is_empty(),
        "these statements are in tests/workloads/nikaya/allow.list and now work at the {} arm, \
         so the entry has to go - a fixed defect left listed reads as coverage and is not:\n  {}",
        arm.name,
        working_after_all.join("\n  ")
    );
    assert_eq!(
        compiled,
        statements.len() - allow.len(),
        "{compiled} of {} statements compiled at the {} arm",
        statements.len(),
        arm.name
    );
    // Rule 1.2: a run where nothing answered would satisfy every assertion
    // above, because "no statement was refused" is true of a loop that refused
    // to run anything. Two thirds of Nikaya's corpus is `SELECT`s, so most of
    // it answers.
    assert!(
        answered * 2 > statements.len(),
        "only {answered} of {} statements answered at the {} arm, with \
         {declined_by_a_constraint} declined by a constraint - so the seed is not building the \
         rows the corpus reads and this is a weaker test than it reports being",
        statements.len(),
        arm.name
    );

    // The corpus is still sound after every statement in it has run, and the
    // seed rows are still there: a statement corpus that quietly deleted its
    // own rows would make every later statement match nothing.
    drop(connection);
    drop(database);
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    let remaining = connection
        .query("SELECT count(*) FROM source_account")
        .expect("the seed table reads back");
    assert_eq!(
        remaining.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Int(1)),
        "the workload left the seed table with something other than its one row, at the {} arm",
        arm.name
    );
}

scenario!(
    every_statement_the_consumer_prepares_runs,
    every_statement_the_consumer_prepares_runs
);
