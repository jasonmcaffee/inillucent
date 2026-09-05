//! Records the pinned release's answers to the SELECT corpus as
//! SQLLogicTest files.
//!
//! Invariant: this tool only ever *writes* expectations, and it only ever gets
//! them from the pinned SQLite binary. Nothing here reads inillucent, so a
//! generated file cannot accidentally record inillucent's own answer as the thing
//! inillucent is graded against - which is the one way a differential corpus can
//! quietly stop testing anything.
//!
//! Usage: `cargo run -p inillucent-compat --bin inillucent-slt`

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::slt::{Record, TestFile};
use inillucent_compat::workspace_root;

/// Generates the conformance files, returning non-zero if it could not.
fn main() -> ExitCode {
    let root = workspace_root();
    match generate(&root) {
        Ok(count) => {
            println!("recorded {count} queries");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the pinned oracle binary, if it has been built.
fn oracle_path(root: &Path) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = root
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Runs the corpus through the oracle and writes the conformance file.
fn generate(root: &Path) -> Result<usize, String> {
    let Some(program) = oracle_path(root) else {
        return Err("the pinned SQLite oracle is not built".to_string());
    };
    let schema = std::fs::read_to_string(root.join("compat/corpus/select/schema.sql"))
        .map_err(|reason| format!("schema.sql: {reason}"))?;
    let queries = std::fs::read_to_string(root.join("compat/corpus/select/queries.sql"))
        .map_err(|reason| format!("queries.sql: {reason}"))?;

    // The built database is a checked-in fixture, exactly like the others: it
    // was written by the pinned SQLite binary, and the runner reads it rather
    // than rebuilding it, so the conformance suite runs with no oracle present.
    let database = root.join("compat/fixtures/select-corpus.db");
    if let Some(parent) = database.parent() {
        std::fs::create_dir_all(parent).map_err(|reason| format!("{reason}"))?;
    }
    let _ = std::fs::remove_file(&database);

    let mut driver = Driver::start("sqlite", &program)?;
    driver.send(&Op::Hello)?;
    driver.send(&Op::Open(database.display().to_string()))?;
    let mut records = Vec::new();
    for statement in split_statements(&schema) {
        let observation = driver.send(&Op::Exec(statement.clone()))?;
        if !observation.ok {
            return Err(format!("{statement}: {}", observation.message));
        }
        records.push(Record::Statement {
            expect_ok: true,
            sql: statement,
        });
    }

    let mut count = 0usize;
    for query in corpus_queries(&queries) {
        let observation = driver.send(&Op::Query(query.clone()))?;
        if !observation.ok {
            return Err(format!("{query}: {}", observation.message));
        }
        let types = column_types(&observation.rows, observation.columns.len());
        let sort = inillucent_compat::slt::sort_mode_for(&query);
        let expected =
            inillucent_compat::slt::apply_sort(render_rows(&observation.rows, &types), sort);
        records.push(Record::Query {
            types,
            sort: sort.to_string(),
            label: None,
            sql: query,
            expected,
        });
        count = count.saturating_add(1);
    }
    let _ = driver.send(&Op::Bye);

    let file = TestFile { records };
    let out = root.join("tests/conformance/select-foundational.test");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|reason| format!("{reason}"))?;
    }
    std::fs::write(&out, file.render()).map_err(|reason| format!("{reason}"))?;
    Ok(count)
}

/// Splits a schema script into statements on semicolons at line ends.
pub fn split_statements(text: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(trimmed);
        if trimmed.ends_with(';') {
            statements.push(current.trim_end_matches(';').trim().to_string());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        statements.push(current.trim().to_string());
    }
    statements
}

/// Returns the corpus queries: one per non-comment line.
pub fn corpus_queries(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .map(str::to_string)
        .collect()
}

/// Derives the type letters from the first row.
///
/// SQLite's own answer decides them, so a column that came back as an integer
/// is compared as an integer; deriving them from the SQL text instead would be
/// a second implementation of type inference. Only the first row is consulted,
/// because a column of a schemaless table holds different classes in different
/// rows and there is no one letter that is right for all of them - `T` renders
/// each value as its own text, which is lossless for every class.
fn column_types(rows: &[Vec<TaggedValue>], width: usize) -> String {
    let Some(first) = rows.first() else {
        // A query that returned no rows still has columns, and the header needs
        // one letter for each of them: an empty letter string renders a header
        // the format cannot read back.
        return "T".repeat(width.max(1));
    };
    first
        .iter()
        .map(|value| match value {
            TaggedValue::Integer(_) => 'I',
            TaggedValue::Real(_) => 'R',
            _ => 'T',
        })
        .collect()
}

/// Renders every value of every row, keeping the rows separate so a sort mode
/// can order them without shuffling one row's columns into another.
fn render_rows(rows: &[Vec<TaggedValue>], types: &str) -> Vec<Vec<String>> {
    let letters: Vec<char> = types.chars().collect();
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(index, value)| {
                    render_tagged(value, letters.get(index).copied().unwrap_or('T'))
                })
                .collect()
        })
        .collect()
}

/// Renders one tagged value the way the format writes it.
fn render_tagged(value: &TaggedValue, letter: char) -> String {
    match value {
        TaggedValue::Null => "NULL".to_string(),
        TaggedValue::Integer(integer) => match letter {
            'R' => inillucent_compat::slt::format_real(*integer as f64),
            _ => integer.to_string(),
        },
        TaggedValue::Real(real) => match letter {
            'I' => (*real as i64).to_string(),
            _ => inillucent_compat::slt::format_real(*real),
        },
        TaggedValue::Text(text) => {
            if text.is_empty() {
                "(empty)".to_string()
            } else {
                String::from_utf8_lossy(text).into_owned()
            }
        }
        TaggedValue::Blob(bytes) => {
            if bytes.is_empty() {
                "(empty)".to_string()
            } else {
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<String>>()
                    .join("")
            }
        }
    }
}
