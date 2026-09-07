//! The dot commands that inspect the database rather than query it.
//!
//! Invariant: each of these answers about *this* engine, and each is written so
//! that its answer can be compared with the reference's byte for byte wherever
//! the question has one answer. `.sha3sum` is the clearest case: the reference
//! hashes a database's **logical content** through a documented byte encoding,
//! not its pages, so two engines holding the same rows produce the same digest -
//! and a digest that did not match would be evidence rather than noise.
//!
//! `.limit`, `.selftest`, `.lint` and `.recover` are the same shape: the
//! question is about the schema, the rows or the limit register, all of which
//! this engine has. `.vfslist` and `.stats` are not, and they say so in their
//! own comments.

use crate::shell::Shell;
use inillucent_value::Value;

/// `.sha3sum ?OPTIONS? ?LIKE-PATTERN?`: a SHA3 over the database's content.
///
/// **Content, not pages.** The reference builds one query per table -
/// `SELECT * FROM "t" NOT INDEXED` - runs them in name order, and hashes every
/// value through a type-tagged encoding: `N` for a null, `I` and eight
/// big-endian bytes for an integer, `F` and eight for a double, `Tnnn:` or
/// `Bnnn:` and the bytes for text or a blob, with an `R` at the head of each
/// row. Two engines holding the same rows therefore agree, which is what makes
/// this worth implementing rather than approximating: it is a checksum two
/// different databases can be compared with.
///
/// @param shell - the shell
/// @param arguments - the options and the optional LIKE pattern
pub fn sha3sum(shell: &mut Shell, arguments: &[&str]) {
    let mut width = 224u32;
    let mut schema = false;
    let mut like: Option<String> = None;
    let mut separate = false;
    for argument in arguments {
        let trimmed = argument.trim_start_matches('-');
        if argument.starts_with('-') {
            match trimmed {
                "schema" => schema = true,
                "sha3-224" | "sha3-256" | "sha3-384" | "sha3-512" => {
                    width = trimmed.get(5..).and_then(|n| n.parse().ok()).unwrap_or(224);
                }
                other => {
                    shell.complain(&format!(
                        "Unknown option \"-{other}\" on \"sha3sum\""
                    ));
                    return;
                }
            }
            continue;
        }
        if like.is_some() {
            shell.complain("Usage: .sha3sum ?OPTIONS? ?LIKE-PATTERN?");
            return;
        }
        like = Some((*argument).to_string());
        separate = true;
    }
    let tables = match hashable_tables(shell, schema) {
        Some(names) => names,
        None => return,
    };
    let mut whole = inillucent_base::sha3::Sha3::new(width);
    let mut lines: Vec<String> = Vec::new();
    for table in tables {
        if let Some(pattern) = &like {
            if !like_matches(pattern, &table) {
                continue;
            }
        }
        let query = content_query(&table);
        if separate {
            let mut one = inillucent_base::sha3::Sha3::new(width);
            if !hash_query(shell, &query, &mut one) {
                return;
            }
            lines.push(format!("{}|{table}", hex(&one.finish())));
        } else if !hash_query(shell, &query, &mut whole) {
            return;
        }
    }
    if separate {
        for line in lines {
            shell.say(&line);
        }
        return;
    }
    let digest = hex(&whole.finish());
    shell.say(&digest);
}

/// Returns the tables `.sha3sum` hashes, in the reference's order.
///
/// Name order under `NOCASE`, which is the reference's `ORDER BY 1 collate
/// nocase`; the internal tables are in only when `--schema` was asked for.
///
/// @param shell - the shell
/// @param schema - whether `sqlite_schema` itself is included
fn hashable_tables(shell: &mut Shell, schema: bool) -> Option<Vec<String>> {
    let sql = if schema {
        "SELECT lower(name) FROM sqlite_schema WHERE type='table' \
          UNION ALL SELECT 'sqlite_schema' ORDER BY 1"
    } else {
        "SELECT lower(name) FROM sqlite_schema WHERE type='table' \
          AND name NOT LIKE 'sqlite__%' ESCAPE '_' ORDER BY 1"
    };
    match shell.collect(sql) {
        Ok((_, rows)) => Some(rows.iter().map(|row| text_of(row.first())).collect()),
        Err(failure) => {
            shell.complain(&format!("Error: {}", failure.message));
            None
        }
    }
}

/// Returns the query whose rows are the table's content, in the reference's
/// wording - including the shapes it gives its own internal tables.
///
/// @param table - the table's folded name
fn content_query(table: &str) -> String {
    // **The trailing semicolon is part of the hash**, not punctuation. The
    // reference prefixes each statement's rows with `S<n>:<sql>`, where `<sql>`
    // is the statement text as prepared - and the text it prepared includes the
    // terminator, because the query string it assembles ends every statement
    // with one. Dropping it changes the digest.
    match table {
        "sqlite_schema" => {
            "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY name;".to_string()
        }
        "sqlite_sequence" => "SELECT name,seq FROM sqlite_sequence ORDER BY name;".to_string(),
        "sqlite_stat1" => "SELECT tbl,idx,stat FROM sqlite_stat1 ORDER BY tbl,idx;".to_string(),
        other => format!("SELECT * FROM \"{}\" NOT INDEXED;", other.replace('"', "\"\"")),
    }
}

/// Runs one query and folds its rows into a sponge.
///
/// @param shell - the shell
/// @param sql - the query
/// @param sponge - the hash being built
fn hash_query(shell: &mut Shell, sql: &str, sponge: &mut inillucent_base::sha3::Sha3) -> bool {
    // The statement's own text goes in first, which is what makes a hash over
    // two tables differ from a hash over their rows run together.
    sponge.update(format!("S{}:", sql.len()).as_bytes());
    sponge.update(sql.as_bytes());
    let rows = match shell.collect(sql) {
        Ok((_, rows)) => rows,
        Err(failure) => {
            shell.complain(&format!("Error: {}", failure.message));
            return false;
        }
    };
    for row in rows {
        sponge.update(b"R");
        for value in &row {
            sponge.update(&encoded(value));
        }
    }
    true
}

/// Returns the bytes one value contributes to a content hash.
///
/// The encoding is the reference's, and every part of it is load-bearing: the
/// type letter keeps `1` and `'1'` apart, the big-endian eight bytes keep the
/// answer independent of the machine, and the `:` after the length keeps a
/// text value that starts with a digit from being confused with a longer one.
///
/// @param value - the column's value
fn encoded(value: &Value<'static>) -> Vec<u8> {
    match value {
        Value::Null => b"N".to_vec(),
        Value::Integer(number) => {
            let mut out = b"I".to_vec();
            out.extend_from_slice(&number.to_be_bytes());
            out
        }
        Value::Real(number) => {
            let mut out = b"F".to_vec();
            out.extend_from_slice(&number.to_bits().to_be_bytes());
            out
        }
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            let mut out = format!("T{}:", bytes.len()).into_bytes();
            out.extend_from_slice(&bytes);
            out
        }
        Value::Blob(blob) => {
            let bytes = blob.raw();
            let mut out = format!("B{}:", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out
        }
    }
}

/// Reports whether a name matches a `LIKE` pattern, case-insensitively.
///
/// @param pattern - the pattern
/// @param name - the table name
fn like_matches(pattern: &str, name: &str) -> bool {
    inillucent_scalar::pattern::like_folding(pattern.as_bytes(), name.as_bytes(), None, true)
}

/// `.limit ?NAME? ?VALUE?`: the run-time limit register.
///
/// The layout is the reference's `%20s %d`, because this is one of the outputs
/// a transcript comparison reads.
///
/// @param shell - the shell
/// @param arguments - a limit name, and a value to set it to
pub fn limit(shell: &mut Shell, arguments: &[&str]) {
    use inillucent_base::limits::Limit;
    // In the reference's order, which is the order of its own table rather than
    // alphabetical or numeric.
    let register: [(&str, Limit); 13] = [
        ("length", Limit::Length),
        ("sql_length", Limit::SqlLength),
        ("column", Limit::Column),
        ("expr_depth", Limit::ExprDepth),
        ("parser_depth", Limit::ParserDepth),
        ("compound_select", Limit::CompoundSelect),
        ("vdbe_op", Limit::VdbeOp),
        ("function_arg", Limit::FunctionArg),
        ("attached", Limit::Attached),
        ("like_pattern_length", Limit::LikePatternLength),
        ("variable_number", Limit::VariableNumber),
        ("trigger_depth", Limit::TriggerDepth),
        ("worker_threads", Limit::WorkerThreads),
    ];
    let limits = inillucent_base::limits::Limits::default();
    let Some(wanted) = arguments.first() else {
        for (name, limit) in register {
            shell.say(&format!("{:>20} {}", name, limits.get(limit)));
        }
        return;
    };
    // The reference matches a prefix and complains when more than one matches,
    // which is what makes `.limit col` work and `.limit c` an error.
    let matched: Vec<&(&str, Limit)> = register
        .iter()
        .filter(|(name, _)| name.starts_with(&wanted.to_ascii_lowercase()))
        .collect();
    match matched.as_slice() {
        [] => shell.complain(&format!(
            "unknown limit: \"{wanted}\"\nenter \".limits\" with no arguments for a list."
        )),
        [(name, limit)] => shell.say(&format!("{:>20} {}", name, limits.get(*limit))),
        _ => shell.complain(&format!("ambiguous limit: \"{wanted}\"")),
    }
}

/// `.selftest ?OPTIONS?`: run the checks a `selftest` table names.
///
/// With no such table there is one default check - `PRAGMA integrity_check`
/// against `ok` - and the reference says so before running it. The wording,
/// including the singular "tests", is the reference's.
///
/// @param shell - the shell
/// @param arguments - unused; `--init` is refused rather than half-done
pub fn selftest(shell: &mut Shell, arguments: &[&str]) {
    if arguments.iter().any(|argument| *argument == "--init") {
        shell.complain("Error: .selftest --init is not implemented by this engine");
        return;
    }
    let has_table = shell
        .collect("SELECT count(*) FROM sqlite_schema WHERE type='table' AND lower(name)='selftest'")
        .ok()
        .and_then(|(_, rows)| rows.first().and_then(|row| row.first()).cloned())
        .map(|value| matches!(value, Value::Integer(count) if count > 0))
        .unwrap_or(false);
    let checks: Vec<(i64, String, String, String)> = if has_table {
        match shell.collect("SELECT tno,op,cmd,ans FROM selftest ORDER BY tno") {
            Ok((_, rows)) => rows
                .iter()
                .map(|row| {
                    (
                        match row.first() {
                            Some(Value::Integer(number)) => *number,
                            _ => 0,
                        },
                        text_of(row.get(1)),
                        text_of(row.get(2)),
                        text_of(row.get(3)),
                    )
                })
                .collect(),
            Err(failure) => {
                shell.complain(&format!("Error: {}", failure.message));
                return;
            }
        }
    } else {
        vec![
            (
                0,
                "memo".to_string(),
                "Missing SELFTEST table - default checks only".to_string(),
                String::new(),
            ),
            (
                1,
                "run".to_string(),
                "PRAGMA integrity_check".to_string(),
                "ok".to_string(),
            ),
        ]
    };
    let mut errors = 0usize;
    let mut tests = 0usize;
    for (tno, operation, command, expected) in checks {
        match operation.as_str() {
            "memo" => shell.say(&command),
            "run" => {
                tests = tests.saturating_add(1);
                match shell.collect(&command) {
                    Ok((_, rows)) => {
                        let got = rows
                            .iter()
                            .flat_map(|row| row.iter().map(|value| text_of(Some(value))))
                            .collect::<Vec<_>>()
                            .join(" ");
                        if got != expected {
                            errors = errors.saturating_add(1);
                            shell.say(&format!("{tno}: Expected: [{expected}]"));
                            shell.say(&format!("{tno}:      Got: [{got}]"));
                        }
                    }
                    Err(failure) => {
                        errors = errors.saturating_add(1);
                        shell.say(&format!("{tno}: error-code-1: {}", failure.message));
                    }
                }
            }
            other => {
                shell.complain(&format!(
                    "Unknown operation \"{other}\" on selftest line {tno}"
                ));
                break;
            }
        }
    }
    shell.say(&format!("{errors} errors out of {tests} tests"));
}

/// `.lint fkey-indexes`: the foreign keys with no index on the child side.
///
/// A foreign key with no index over the child's columns turns every delete or
/// update of a parent row into a scan of the child table, and nothing in the
/// schema says so - which is why it is worth a command rather than a comment.
/// The output is the `CREATE INDEX` the reference suggests, with the parent it
/// is for named after it.
///
/// @param shell - the shell
/// @param arguments - which lint to run
pub fn lint(shell: &mut Shell, arguments: &[&str]) {
    let Some(which) = arguments.first() else {
        shell.complain("Usage: .lint fkey-indexes");
        return;
    };
    if !"fkey-indexes".starts_with(which) {
        shell.complain(&format!("Error: unknown lint: \"{which}\""));
        return;
    }
    let Ok((_, tables)) = shell.collect(
        "SELECT name FROM sqlite_schema WHERE type='table' \
          AND name NOT LIKE 'sqlite__%' ESCAPE '_' ORDER BY name",
    ) else {
        shell.complain("Error: could not read the schema");
        return;
    };
    let names: Vec<String> = tables.iter().map(|row| text_of(row.first())).collect();
    for child in names {
        let quoted = child.replace('\'', "''");
        let Ok((_, keys)) = shell.collect(&format!(
            "SELECT id, seq, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list('{quoted}') \
              ORDER BY id, seq"
        )) else {
            continue;
        };
        // One row per column of a key, so the columns are gathered per key id
        // before anything is decided about it.
        let mut grouped: Vec<(i64, String, Vec<String>)> = Vec::new();
        for row in &keys {
            let id = match row.first() {
                Some(Value::Integer(number)) => *number,
                _ => 0,
            };
            let parent = text_of(row.get(2));
            let column = text_of(row.get(3));
            match grouped.last_mut() {
                Some((held, _, columns)) if *held == id => columns.push(column),
                _ => grouped.push((id, parent, vec![column])),
            }
        }
        for (_, parent, columns) in grouped {
            if child_is_indexed(shell, &child, &columns) {
                continue;
            }
            let parent_key = parent_key_columns(shell, &parent);
            shell.say(&format!(
                "CREATE INDEX '{}_{}' ON '{}'({}); --> {}({})",
                child,
                columns.join("_"),
                child,
                columns
                    .iter()
                    .map(|column| format!("'{column}'"))
                    .collect::<Vec<_>>()
                    .join(", "),
                parent,
                parent_key
            ));
        }
    }
}

/// Reports whether an index already covers a key's columns as its prefix.
///
/// A prefix is enough: an index on `(a, b)` serves a key on `a`, which is why
/// this compares the leading columns rather than the whole list.
///
/// @param shell - the shell
/// @param table - the child table
/// @param columns - the key's columns, in key order
fn child_is_indexed(shell: &mut Shell, table: &str, columns: &[String]) -> bool {
    let quoted = table.replace('\'', "''");
    let Ok((_, indexes)) = shell.collect(&format!(
        "SELECT name FROM pragma_index_list('{quoted}') ORDER BY seq"
    )) else {
        return false;
    };
    for row in &indexes {
        let index = text_of(row.first()).replace('\'', "''");
        let Ok((_, entries)) = shell.collect(&format!(
            "SELECT name FROM pragma_index_info('{index}') ORDER BY seqno"
        )) else {
            continue;
        };
        let held: Vec<String> = entries.iter().map(|row| text_of(row.first())).collect();
        if held.len() >= columns.len()
            && held
                .iter()
                .zip(columns.iter())
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        {
            return true;
        }
    }
    false
}

/// Returns the parent columns a key points at, as the suggestion names them.
///
/// @param shell - the shell
/// @param parent - the parent table
fn parent_key_columns(shell: &mut Shell, parent: &str) -> String {
    let quoted = parent.replace('\'', "''");
    let Ok((_, columns)) = shell.collect(&format!(
        "SELECT name FROM pragma_table_info('{quoted}') WHERE pk > 0 ORDER BY pk"
    )) else {
        return String::new();
    };
    columns
        .iter()
        .map(|row| text_of(row.first()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns a value as plain text.
///
/// @param value - the value, when there is one
fn text_of(value: Option<&Value<'static>>) -> String {
    match value {
        Some(Value::Text(text)) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
        Some(Value::Integer(number)) => number.to_string(),
        Some(Value::Real(number)) => crate::render::literal(&Value::Real(*number)),
        Some(Value::Blob(blob)) => String::from_utf8_lossy(blob.raw()).into_owned(),
        Some(Value::Null) | None => String::new(),
    }
}

/// Renders bytes as lowercase hex.
///
/// @param bytes - the digest
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `.recover`: the SQL that rebuilds as much of the database as can be read.
///
/// **A salvage walk, not a dump.** `.dump` reads the schema and then queries
/// every table through the ordinary path, so a database with one damaged tree
/// produces no output at all past the point it fails. `.recover` reads what it
/// can and writes it out as statements that will not fight each other on the way
/// back in: `writable_schema` on so the schema rows go in whatever order they
/// come, `foreign_keys` off so a child may arrive before its parent, and
/// `INSERT OR IGNORE` so a duplicate that survived the damage does not stop the
/// rest of the table.
///
/// The header pragmas are written as the reference writes them - quoted values
/// included - because the output of this command is a file people diff.
///
/// @param shell - the shell
/// @param arguments - unused; the reference's options are not implemented
pub fn recover(shell: &mut Shell, arguments: &[&str]) {
    for argument in arguments {
        if argument.starts_with('-') {
            shell.complain(&format!("Error: unknown option \"{argument}\" on \".recover\""));
            return;
        }
    }
    shell.say(".dbconfig defensive off");
    shell.say("BEGIN;");
    shell.say("PRAGMA writable_schema = on;");
    shell.say("PRAGMA foreign_keys = off;");
    for (name, pragma) in [
        ("encoding", "PRAGMA encoding"),
        ("page_size", "PRAGMA page_size"),
        ("auto_vacuum", "PRAGMA auto_vacuum"),
        ("user_version", "PRAGMA user_version"),
        ("application_id", "PRAGMA application_id"),
    ] {
        let value = shell.scalar(pragma).unwrap_or_default();
        shell.say(&format!("PRAGMA {name} = '{value}';"));
    }
    let Ok((_, objects)) = shell.collect(
        "SELECT type, name, sql FROM sqlite_schema \
          WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite__%' ESCAPE '_' \
          ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, rowid",
    ) else {
        shell.complain("Error: could not read the schema");
        return;
    };
    let described: Vec<(String, String, String)> = objects
        .iter()
        .map(|row| {
            (
                text_of(row.first()),
                text_of(row.get(1)),
                text_of(row.get(2)),
            )
        })
        .collect();
    for (kind, name, sql) in &described {
        shell.say(&format!("{sql};"));
        if kind != "table" {
            continue;
        }
        recover_rows(shell, name);
    }
    shell.say("PRAGMA writable_schema = off;");
    shell.say("COMMIT;");
}

/// Writes one table's rows as `INSERT OR IGNORE` statements.
///
/// A table whose rows cannot be read at all is skipped in silence rather than
/// aborting the walk, which is the whole difference between this and `.dump`:
/// the point of a salvage is what it *can* recover.
///
/// @param shell - the shell
/// @param table - the table's name, as written
fn recover_rows(shell: &mut Shell, table: &str) {
    let quoted = table.replace('\'', "''");
    let Ok((columns, rows)) = shell.collect(&format!(
        "SELECT * FROM \"{}\"",
        table.replace('"', "\"\"")
    )) else {
        return;
    };
    if rows.is_empty() {
        return;
    }
    let names = columns
        .iter()
        .map(|column| format!("'{}'", column.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    for row in rows {
        let values = row
            .iter()
            .map(crate::render::literal)
            .collect::<Vec<_>>()
            .join(", ");
        shell.say(&format!(
            "INSERT OR IGNORE INTO '{quoted}'({names}) VALUES ({values});"
        ));
    }
}
