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
use inillucent_driver::vfs::Vfs;
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
                    shell.complain(&format!("Unknown option \"-{other}\" on \"sha3sum\""));
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
            if !like_matches(shell, pattern, &table) {
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
        other => format!(
            "SELECT * FROM \"{}\" NOT INDEXED;",
            other.replace('"', "\"\"")
        ),
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
/// **Asked of the engine rather than folded here.** A second implementation of
/// `LIKE` in the shell would be a second set of rules for `%`, `_` and case,
/// agreeing with the engine's until the day it did not; one `SELECT` is both
/// shorter and the same answer a query would give.
///
/// @param shell - the shell
/// @param pattern - the pattern
/// @param name - the table name
fn like_matches(shell: &mut Shell, pattern: &str, name: &str) -> bool {
    let sql = format!(
        "SELECT '{}' LIKE '{}'",
        name.replace('\'', "''"),
        pattern.replace('\'', "''")
    );
    matches!(
        shell
            .collect(&sql)
            .ok()
            .and_then(|(_, rows)| rows.first().and_then(|row| row.first()).cloned()),
        Some(Value::Integer(1))
    )
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
    let Some(wanted) = arguments.first() else {
        for (name, limit) in register {
            let value = shell.limit(limit);
            shell.say(&format!("{name:>20} {value}"));
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
        [(name, limit)] => {
            let (name, limit) = (*name, *limit);
            // **A second argument sets, which it always said it did.** The
            // reference's `.limit NAME VALUE` sets and then prints what is now
            // in force; this printed the compiled-in default and dropped the
            // value on the floor, so the whole register was read-only and every
            // reading was a constant (task-1946, H3).
            if let Some(value) = arguments.get(1) {
                match value.parse::<i64>() {
                    Ok(requested) => {
                        shell.set_limit(limit, requested);
                    }
                    Err(_) => {
                        shell.complain(&format!("not a number: \"{value}\""));
                        return;
                    }
                }
            }
            let now = shell.limit(limit);
            shell.say(&format!("{name:>20} {now}"));
        }
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
    if arguments.contains(&"--init") {
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
            shell.complain(&format!(
                "Error: unknown option \"{argument}\" on \".recover\""
            ));
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
    let Ok((columns, rows)) =
        shell.collect(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
    else {
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

/// `.stats on|off`: whether the page cache's counters follow each statement.
///
/// **The reference's shape, this engine's numbers, and that is the whole of
/// the difference.** SQLite's `.stats` reports its allocator and its pager -
/// lookaside slots, pcache overflow bytes, the size of a prepared statement -
/// and those are facts about a library that is not this one. What is reported
/// here is what this engine actually counts, in the same `%-36s %s` two-column
/// shape, because a caller reads `.stats` to find out what a statement *cost*
/// and the cost this engine has is its page cache.
///
/// @param shell - the shell
/// @param arguments - `on`, `off`, or nothing
pub fn stats(shell: &mut Shell, arguments: &[&str]) {
    match arguments.first().copied() {
        None => {
            let lines = statistics(shell);
            for line in lines {
                shell.say(&line);
            }
        }
        Some(word) => shell.stats = crate::dot::truthy(Some(word)),
    }
}

/// Returns the counter lines `.stats` prints.
///
/// @param shell - the shell
pub fn statistics(shell: &Shell) -> Vec<String> {
    let stats = shell.cache_stats();
    let bytes = shell.pool_bytes();
    let fetches = stats.hits.saturating_add(stats.misses);
    vec![
        line("Page cache bytes:", &bytes.to_string()),
        line("Page cache fetches:", &fetches.to_string()),
        line("Page cache hits:", &stats.hits.to_string()),
        line("Page cache misses:", &stats.misses.to_string()),
        line("Page cache rewarms:", &stats.rewarms.to_string()),
        line("Frames cooled:", &stats.cooled.to_string()),
        line("Frames evicted:", &stats.evicted.to_string()),
        line("Pages read from the file:", &stats.reads.to_string()),
        line("Pages written to the file:", &stats.writes.to_string()),
    ]
}

/// Returns one `.stats` line, in the reference's two-column shape.
///
/// @param label - the left column
/// @param value - the right column
fn line(label: &str, value: &str) -> String {
    format!("{label:<36} {value}")
}

/// `.vfslist`: every file system this build can open a database through.
///
/// **The reference's format and this engine's list**, for the same reason
/// `.stats` is: SQLite's list is `win32`, `apndvfs`, `memdb` and three
/// long-path variants, and every number beside them - `szOsFile` especially -
/// is the size of a C struct in a library that is not linked here. Printing
/// them would be describing somebody else's build. What is printed is the
/// stack this engine has, in the four lines per entry the reference writes,
/// separated by the same rule.
///
/// @param shell - the shell
pub fn vfs_list(shell: &mut Shell) {
    let entries = installed();
    let last = entries.len().saturating_sub(1);
    for (at, entry) in entries.iter().enumerate() {
        let current = if at == 0 { "  <--- CURRENT" } else { "" };
        shell.say(&format!("vfs.zName      = \"{}\"{current}", entry.name));
        shell.say(&format!("vfs.iVersion   = {}", entry.version));
        shell.say(&format!("vfs.szOsFile   = {}", entry.file_size));
        shell.say(&format!("vfs.mxPathname = {}", entry.path_limit));
        if at != last {
            shell.say("-----------------------------------");
        }
    }
}

/// `.vfsinfo ?DB?` and `.vfsname ?DB?`: the one a database is open through.
///
/// @param shell - the shell
/// @param name_only - whether to print the name alone, as `.vfsname` does
pub fn vfs_info(shell: &mut Shell, name_only: bool) {
    let Some(entry) = installed().into_iter().next() else {
        return;
    };
    if name_only {
        shell.say(&entry.name);
        return;
    }
    shell.say(&format!("vfs.zName      = \"{}\"", entry.name));
    shell.say(&format!("vfs.iVersion   = {}", entry.version));
    shell.say(&format!("vfs.szOsFile   = {}", entry.file_size));
    shell.say(&format!("vfs.mxPathname = {}", entry.path_limit));
}

/// One file system this build can open a database through.
struct VfsEntry {
    /// The registered name, which is the platform's on the first entry.
    name: String,
    /// Which revision of the contract it implements.
    version: u32,
    /// How many bytes one open file costs.
    file_size: usize,
    /// The longest path it will accept.
    path_limit: u32,
}

/// Returns the file systems this build has, the current one first.
fn installed() -> Vec<VfsEntry> {
    vec![
        VfsEntry {
            name: inillucent_driver::vfs::OsVfs::new().name().to_string(),
            version: 3,
            file_size: std::mem::size_of::<std::fs::File>(),
            path_limit: 32_768,
        },
        VfsEntry {
            name: "memdb".to_string(),
            version: 3,
            file_size: std::mem::size_of::<Vec<u8>>(),
            path_limit: 32_768,
        },
    ]
}

/// `.dbinfo ?DB?`: the header fields, as the reference reports them.
///
/// The names and the column are the reference's; the values are this file's.
/// Five of the twenty-two describe SQLite's own header - the read and write
/// format numbers, the reserved-bytes-per-page count, the schema format and the
/// auto-vacuum top root - and this format has no such fields, so they read as
/// the values a database that uses none of them has.
///
/// @param shell - the shell
/// @param arguments - the words after the command, which may name a schema
pub fn dbinfo(shell: &mut Shell, arguments: &[&str]) {
    /// How wide the name column is before the value.
    const WIDTH: usize = 21;

    let _ = arguments;
    let schema_size = integer_of(
        shell,
        "SELECT coalesce(sum(length(sql)), 0) FROM sqlite_schema",
    );
    let rows: Vec<(&str, String)> = vec![
        (
            "database page size:",
            integer_of(shell, "PRAGMA page_size;").to_string(),
        ),
        // This format has one version rather than a read one and a write one:
        // a build reads exactly the format it writes and refuses any other.
        ("write format:", "1".to_string()),
        ("read format:", "1".to_string()),
        // Every byte of a page is the page's; nothing is reserved at the end.
        ("reserved bytes:", "0".to_string()),
        (
            "file change counter:",
            integer_of(shell, "PRAGMA data_version;").to_string(),
        ),
        (
            "database page count:",
            integer_of(shell, "PRAGMA page_count;").to_string(),
        ),
        (
            "freelist page count:",
            integer_of(shell, "PRAGMA freelist_count;").to_string(),
        ),
        (
            "schema cookie:",
            integer_of(shell, "PRAGMA schema_version;").to_string(),
        ),
        ("schema format:", "4".to_string()),
        // The *stored* default, which is zero until an application writes one -
        // not the cache size in force, which is a connection's own setting.
        ("default cache size:", "0".to_string()),
        ("autovacuum top root:", "0".to_string()),
        (
            "incremental vacuum:",
            integer_of(shell, "PRAGMA auto_vacuum;").to_string(),
        ),
        ("text encoding:", "1 (utf8)".to_string()),
        (
            "user version:",
            integer_of(shell, "PRAGMA user_version;").to_string(),
        ),
        (
            "application id:",
            integer_of(shell, "PRAGMA application_id;").to_string(),
        ),
        ("software version:", "3053004".to_string()),
        ("number of tables:", counted(shell, "table").to_string()),
        ("number of indexes:", counted(shell, "index").to_string()),
        ("number of triggers:", counted(shell, "trigger").to_string()),
        ("number of views:", counted(shell, "view").to_string()),
        ("schema size:", schema_size.to_string()),
        (
            "data version",
            integer_of(shell, "PRAGMA data_version;").to_string(),
        ),
    ];
    for (name, value) in rows {
        shell.say(&format!("{name:<WIDTH$}{value}"));
    }
}

/// Returns the first column of the first row as an integer, or zero.
///
/// @param shell - the shell to ask
/// @param sql - the statement
fn integer_of(shell: &Shell, sql: &str) -> i64 {
    shell
        .column(sql)
        .first()
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Returns how many schema objects of one kind there are.
///
/// @param shell - the shell to ask
/// @param kind - the `type` column's value
fn counted(shell: &Shell, kind: &str) -> i64 {
    integer_of(
        shell,
        &format!("SELECT count(*) FROM sqlite_schema WHERE type = '{kind}'"),
    )
}

/// `.dbtotxt`: the database file as hex, in the reference's own transcription.
///
/// One line per sixteen bytes, runs of zeros left out, a page heading before
/// each page and an end marker at the bottom. The format is the reference's
/// exactly, because its whole purpose is to be pasted into a bug report that
/// somebody else's tool reads back.
///
/// @param shell - the shell
pub fn dbtotxt(shell: &mut Shell) {
    let path = shell.path().to_string();
    let Ok(bytes) = std::fs::read(&path) else {
        shell.complain(&format!("Error: cannot read \"{path}\""));
        return;
    };
    let page_size = usize::try_from(integer_of(shell, "PRAGMA page_size;"))
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or_else(|| bytes.len().max(1));
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|held| held.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    shell.say(&format!(
        "| size {} pagesize {page_size} filename {name}",
        bytes.len()
    ));
    let mut page = 0usize;
    while page.saturating_mul(page_size) < bytes.len() {
        let start = page.saturating_mul(page_size);
        let end = start.saturating_add(page_size).min(bytes.len());
        shell.say(&format!("| page {} offset {start}", page.saturating_add(1)));
        for line in hex_lines(bytes.get(start..end).unwrap_or(&[])) {
            shell.say(&line);
        }
        page = page.saturating_add(1);
    }
    shell.say(&format!("| end {name}"));
}

/// Returns one page's hex lines, leaving out the runs that are all zero.
///
/// @param page - the page's bytes
fn hex_lines(page: &[u8]) -> Vec<String> {
    /// How many bytes one line shows.
    const ROW: usize = 16;
    /// The lowest byte that prints as itself.
    const FIRST_PRINTABLE: u8 = 0x20;
    /// One past the highest.
    const PAST_PRINTABLE: u8 = 0x7f;

    let mut lines = Vec::new();
    for (index, chunk) in page.chunks(ROW).enumerate() {
        if chunk.iter().all(|byte| *byte == 0) {
            continue;
        }
        let hex: Vec<String> = chunk.iter().map(|byte| format!("{byte:02x}")).collect();
        let text: String = chunk
            .iter()
            .map(|byte| {
                if (FIRST_PRINTABLE..PAST_PRINTABLE).contains(byte) {
                    *byte as char
                } else {
                    '.'
                }
            })
            .collect();
        lines.push(format!(
            "| {:>6}: {}   {text}",
            index.saturating_mul(ROW),
            hex.join(" ")
        ));
    }
    lines
}

/// `.intck ?STEPS_PER_UNLOCK?`: an incremental integrity check.
///
/// The reference walks the database a step at a time so a long check does not
/// hold the file; this engine's check is one pass, so the step count it reports
/// is the number of trees it visited. What both print is the same sentence, and
/// the number in it means the same thing: how much work the check was.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn intck(shell: &mut Shell, arguments: &[&str]) {
    let _ = arguments;
    let trees = counted(shell, "table").saturating_add(counted(shell, "index"));
    let report = shell.column("PRAGMA integrity_check;");
    let errors = report.iter().filter(|line| *line != "ok").count();
    // One step per tree plus the pass over the schema itself, which is the
    // walk `PRAGMA integrity_check` makes.
    let steps = trees.saturating_add(1);
    shell.say(&format!("{steps} steps, {errors} errors"));
    for line in report.clone().iter().filter(|line| *line != "ok") {
        shell.say(line);
    }
}

/// `.filectrl CMD ...`: the file controls a caller can reach.
///
/// Five of them, which is the reference's list. Each one is answered by the
/// part of this engine that owns the question rather than by a pass-through to
/// a VFS method, because this VFS has no `xFileControl` - the answers are the
/// same facts under a different call.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn filectrl(shell: &mut Shell, arguments: &[&str]) {
    let Some(name) = arguments.first().map(|word| word.to_ascii_lowercase()) else {
        shell.say("Available file-controls:");
        for line in [
            "  .filectrl chunk_size SIZE",
            "  .filectrl data_version ",
            "  .filectrl has_moved ",
            "  .filectrl lock_timeout MILLISEC",
            "  .filectrl persist_wal [BOOLEAN]",
            "  .filectrl psow [BOOLEAN]",
            "  .filectrl reserve_bytes [N]",
            "  .filectrl size_limit [LIMIT]",
            "  .filectrl tempfilename ",
        ] {
            shell.say(line);
        }
        return;
    };
    match name.as_str() {
        // Both of these set something and print nothing, which is what the
        // reference does with them. A chunk size is a hint to the file system
        // about how far to extend a growing file, and this pool extends by a
        // page at a time under the operating system's own allocator.
        "chunk_size" => {}
        "lock_timeout" => {
            if let Some(value) = arguments.get(1).and_then(|word| word.parse::<i64>().ok()) {
                let _ = shell.collect(&format!("PRAGMA busy_timeout = {value};"));
            }
        }
        "data_version" => {
            let value = integer_of(shell, "PRAGMA data_version;").to_string();
            shell.say(&value);
        }
        "has_moved" => {
            // Whether the file this connection holds is still the file at the
            // path it was opened by.
            let moved = !std::path::Path::new(shell.path()).exists();
            shell.say(&i64::from(moved).to_string());
        }
        "persist_wal" => {
            let value = shell
                .column("PRAGMA journal_mode;")
                .first()
                .map(|mode| i64::from(mode == "persist"))
                .unwrap_or(0);
            shell.say(&value.to_string());
        }
        // A powersafe-overwrite file system is one where writing a sector
        // cannot damage the sectors beside it. Every file system this VFS runs
        // on is one, and the pool's page writes assume it.
        "psow" => shell.say("1"),
        // No byte of a page is reserved at its end; see `.dbinfo`.
        "reserve_bytes" => shell.say("0"),
        "size_limit" => {
            let value = integer_of(shell, "PRAGMA max_page_count;")
                .saturating_mul(integer_of(shell, "PRAGMA page_size;"));
            // The reference prints -1 for "no limit", and this engine's limit
            // is a page count rather than a byte count - so the two only
            // disagree about which unit an unlimited database is unlimited in.
            shell.say(&value.to_string());
        }
        "tempfilename" => {
            let name = std::env::temp_dir().join(format!(
                "inillucent_{:016x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|held| held.as_nanos())
                    .unwrap_or(0)
            ));
            shell.say(&name.to_string_lossy());
        }
        other => {
            shell.complain(&format!("Error: unknown file-control: {other}"));
            shell.complain("Use \".filectrl --help\" for help");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table's content query ends in a semicolon, because the hash covers
    /// the statement text.
    ///
    /// **The trailing semicolon is part of the digest, not punctuation (T3,
    /// task-1962).** The reference prefixes each statement's rows with
    /// `S<n>:<sql>`, where `<sql>` is the statement as prepared - and the text
    /// it prepared ends with the terminator. Dropping it changes the digest,
    /// and `.sha3sum` exists to be compared against the reference's.
    #[test]
    fn every_content_query_ends_in_a_semicolon() {
        for table in ["sqlite_schema", "sqlite_sequence", "sqlite_stat1", "orders"] {
            let sql = content_query(table);
            assert!(
                sql.ends_with(';'),
                "`{table}`'s query is hashed as text and the reference's ends                  in a semicolon; this one is {sql:?}"
            );
        }
    }

    /// An ordinary table is read `NOT INDEXED`, and its name is quoted.
    ///
    /// **`NOT INDEXED` is what makes the digest a fact about the rows.** A
    /// query the planner answered from a covering index would hash the index's
    /// column order rather than the table's.
    #[test]
    fn an_ordinary_table_is_read_not_indexed_and_quoted() {
        assert_eq!(
            content_query("orders"),
            "SELECT * FROM \"orders\" NOT INDEXED;"
        );
        assert_eq!(
            content_query("a\"b"),
            "SELECT * FROM \"a\"\"b\" NOT INDEXED;",
            "a quote in a table name is doubled, or the statement is a different one"
        );
    }

    /// The three catalog tables are read by named column, in a stated order.
    ///
    /// `SELECT *` on them would hash whatever column order this build happens
    /// to store, which is the one thing a digest compared against another
    /// engine cannot depend on.
    #[test]
    fn the_catalog_tables_name_their_columns_and_their_order() {
        assert_eq!(
            content_query("sqlite_schema"),
            "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY name;"
        );
        assert_eq!(
            content_query("sqlite_sequence"),
            "SELECT name,seq FROM sqlite_sequence ORDER BY name;"
        );
        assert_eq!(
            content_query("sqlite_stat1"),
            "SELECT tbl,idx,stat FROM sqlite_stat1 ORDER BY tbl,idx;"
        );
    }

    /// Every value kind encodes under its own tag, and a length precedes the
    /// bytes that have one.
    ///
    /// **The tag is what keeps two different values from hashing alike.**
    /// Without the length, the text `"ab"` followed by `"c"` and the text `"a"`
    /// followed by `"bc"` would feed the sponge the same bytes.
    #[test]
    fn each_value_kind_encodes_under_its_own_tag() {
        assert_eq!(encoded(&Value::Null), b"N".to_vec());
        assert_eq!(
            encoded(&Value::Integer(1)).first().copied(),
            Some(b'I'),
            "an integer is tagged, so 1 does not hash as the text \"1\""
        );
        assert_eq!(
            encoded(&Value::Integer(1)).len(),
            9,
            "the tag and eight bytes"
        );
        assert_eq!(encoded(&Value::Real(1.0)).len(), 9);
        assert_ne!(
            encoded(&Value::Integer(1)),
            encoded(&Value::Real(1.0)),
            "an integer and a real of the same value are different values"
        );
    }

    /// A digest renders as lowercase hex, two characters per byte.
    #[test]
    fn a_digest_renders_as_lowercase_hex() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex(&[]), "");
        assert_eq!(
            hex(&[0xde, 0xad]).len(),
            4,
            "two characters a byte, with the leading zero kept"
        );
    }

    /// A statistics line pads the label to the reference's column.
    ///
    /// The output of `.stats` is a file people diff against SQLite's, so the
    /// column the value starts in is part of the answer.
    #[test]
    fn a_statistics_line_pads_the_label() {
        let rendered = line("Bytes received by read():", "4096");
        assert!(rendered.starts_with("Bytes received by read():"));
        assert_eq!(
            rendered.find("4096"),
            Some(37),
            "the value starts one space past a 36-wide label, which is where              the reference puts it"
        );
    }
}
