//! `.import`: reading a delimited file into a table.
//!
//! Invariant: a field is a string unless the table says otherwise. The import
//! does not guess types - it binds every field as text and lets the column's
//! affinity decide, which is exactly what an `INSERT` with a string literal
//! would do. Guessing here would make `.import` and `INSERT` disagree about the
//! same file, and the one that guessed would be the one that was wrong.
//!
//! The separator is whatever `.separator` is set to unless an option overrides
//! it, and in `csv` mode the quoting rules are CSV's: a field may be quoted, a
//! doubled quote inside a quoted field is one quote, and a separator inside
//! quotes is data.
//!
//! **The options are parsed, not skipped.** They were not, until task-1907, and
//! the cost was that `inillucent import` - the verb, which builds the line
//! `.import --csv "file" "table"` - failed on every file anybody gave it with
//! `cannot open "--csv"`, because the first word was read as the file name. The
//! shape of that bug is worth keeping in mind: the dot command was tested
//! through a script that passed no options, so the one caller that always
//! passes one was the only caller that never worked.

use crate::render::Mode;
use crate::shell::Shell;

/// The unit and record separators `--ascii` reads, which are what `.mode ascii`
/// writes.
const UNIT_SEPARATOR: char = '\u{1f}';
const RECORD_SEPARATOR: char = '\u{1e}';

/// How one `.import` reads its file, after its options have been read.
struct Reading<'a> {
    /// The file to read.
    path: &'a str,
    /// The table to load into.
    table: &'a str,
    /// What separates two fields.
    separator: char,
    /// What ends a row.
    row_separator: char,
    /// Whether a field may be quoted, RFC 4180 style.
    quoted: bool,
    /// How many rows to drop off the front, header included.
    skip: usize,
}

/// Reads `.import`'s options and the two names that follow them.
///
/// Every option this command's own help text documents is understood here, and
/// an option it does not understand is refused by name. Reading an unknown
/// option as the file name is the failure this function exists to prevent: it
/// produces `cannot open "--esc"`, which sends a reader looking for a missing
/// file rather than at the flag they mistyped.
///
/// @param shell - what the defaults are read from, and what a complaint goes to
/// @param arguments - the words after `.import`
fn reading<'a>(shell: &mut Shell, arguments: &[&'a str]) -> Option<Reading<'a>> {
    let mut separator = if shell.layout.mode == Mode::Csv {
        ','
    } else {
        shell.layout.separator.chars().next().unwrap_or('|')
    };
    let mut quoted = shell.layout.mode == Mode::Csv;
    let mut row_separator = '\n';
    let mut skip = 0usize;
    let mut positional: Vec<&'a str> = Vec::new();

    let mut index = 0;
    while index < arguments.len() {
        let Some(word) = arguments.get(index).copied() else {
            break;
        };
        index += 1;
        // An option that takes a value consumes the next word here, so that
        // word can never be mistaken for the file name further down.
        let value = if matches!(word, "--colsep" | "--rowsep" | "--skip") {
            let Some(taken) = arguments.get(index).copied() else {
                shell.complain(&format!("Error: {word} wants a value"));
                return None;
            };
            index += 1;
            taken
        } else {
            ""
        };
        match word {
            "--csv" => {
                separator = ',';
                quoted = true;
                row_separator = '\n';
            }
            "--ascii" => {
                separator = UNIT_SEPARATOR;
                quoted = false;
                row_separator = RECORD_SEPARATOR;
            }
            // SQLite takes the first character of a longer separator. An empty
            // one is refused rather than defaulted, because a separator that is
            // not the one asked for loads a file whose fields are all wrong and
            // reports nothing.
            "--colsep" | "--rowsep" => {
                let Some(character) = value.chars().next() else {
                    shell.complain(&format!("Error: {word} wants a character"));
                    return None;
                };
                if word == "--colsep" {
                    separator = character;
                } else {
                    row_separator = character;
                }
            }
            "--skip" => match value.parse::<usize>() {
                Ok(count) => skip = count,
                Err(_) => {
                    shell.complain("Error: --skip wants a number of rows");
                    return None;
                }
            },
            "-v" => {}
            // `--schema`, `--esc` and `--qesc` are in SQLite's help for this
            // command and are not built here. They are refused rather than
            // ignored: an escape character that is accepted and not applied
            // loads a file whose fields are silently wrong.
            other if other.starts_with('-') => {
                shell.complain(&format!("Error: .import does not take {other}"));
                return None;
            }
            other => positional.push(other),
        }
    }

    let (Some(path), Some(table)) = (positional.first(), positional.get(1)) else {
        shell.complain("Error: .import requires a file name and a table name");
        return None;
    };
    Some(Reading {
        path,
        table,
        separator,
        row_separator,
        quoted,
        skip,
    })
}

/// Reads a file into a table.
pub fn import(shell: &mut Shell, arguments: &[&str]) {
    let Some(reading) = reading(shell, arguments) else {
        return;
    };
    let (named, table) = (reading.path, reading.table);
    // **Through the same confinement function every other path-taking dot
    // command calls (task-1979, H1).** This reads its file with `std::fs`, so
    // the confined VFS never sees the path and `--root` did not apply to it.
    let Some(path) = crate::dot::confine_path(shell, named) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        shell.complain(&format!("Error: cannot open \"{named}\""));
        return;
    };
    let mut rows = parse(
        &text,
        reading.separator,
        reading.quoted,
        reading.row_separator,
    );
    if reading.skip > 0 {
        rows.drain(..reading.skip.min(rows.len()));
    }
    if rows.is_empty() {
        return;
    }
    let exists = table_exists(shell, table);
    if !exists {
        // With no table, the first line is the header and the columns are all
        // text - which is what SQLite's shell does, and is the only thing it
        // can do without being told the types.
        let header = rows.remove(0);
        let columns: Vec<String> = header.iter().map(|name| quote_identifier(name)).collect();
        let create = format!(
            "CREATE TABLE {} ({})",
            quote_identifier(table),
            columns.join(",")
        );
        if let Err(message) = shell.execute(&create) {
            shell.complain(&format!("Error: {message}"));
            return;
        }
    }
    insert(shell, table, &rows, &path);
}

/// Inserts every parsed row, reporting the first line that will not go in.
///
/// One prepared statement for the whole file. Compiling an `INSERT` per row is
/// the obvious way to write this and it is the wrong one: the statement is
/// identical every time, and preparing it five thousand times is five thousand
/// compilations of the same text.
fn insert(shell: &mut Shell, table: &str, rows: &[Vec<String>], path: &str) {
    let width = rows.first().map_or(0, Vec::len);
    if width == 0 {
        return;
    }
    let marks: Vec<&str> = std::iter::repeat_n("?", width).collect();
    let sql = format!(
        "INSERT INTO {} VALUES({})",
        quote_identifier(table),
        marks.join(",")
    );
    if let Err(message) = shell.execute("BEGIN") {
        shell.complain(&format!("Error: {message}"));
        return;
    }
    let failure = fill(&shell.connection(), &sql, rows);
    match failure {
        None => {
            if let Err(message) = shell.execute("COMMIT") {
                shell.complain(&format!("Error: {message}"));
            }
        }
        Some((line, message)) => {
            shell.complain(&format!("{path}:{line}: {message}"));
            let _ = shell.execute("ROLLBACK");
        }
    }
}

/// Binds and runs every row, returning the first line that would not go in.
///
/// It borrows the connection for the whole loop, which is why it reports rather
/// than complains: the shell cannot be borrowed mutably while a statement it
/// prepared is alive.
fn fill(
    connection: &inillucent_engine::connect::Connection<'_>,
    sql: &str,
    rows: &[Vec<String>],
) -> Option<(usize, String)> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(error) => return Some((0, error.message().to_string())),
    };
    for (index, row) in rows.iter().enumerate() {
        // Every row starts from nothing bound, so a row shorter than the one
        // before it does not inherit the tail of that one.
        statement.clear_bindings();
        for (position, field) in row.iter().enumerate() {
            if let Err(error) = statement.bind_text(position as u32 + 1, field) {
                return Some((index + 1, error.message().to_string()));
            }
        }
        // A row shorter than the first one leaves the rest NULL, which is what
        // an `INSERT` with fewer values would have done - and what an unbound
        // parameter already is, so there is nothing to write for them.
        loop {
            match statement.step() {
                Ok(true) => continue,
                Ok(false) => break,
                Err(error) => return Some((index + 1, error.message().to_string())),
            }
        }
    }
    None
}

/// Reports whether a table is already there.
fn table_exists(shell: &Shell, table: &str) -> bool {
    let sql = format!(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = '{}'",
        table.replace('\'', "''")
    );
    shell.scalar(&sql).is_some_and(|count| count != "0")
}

/// Splits a file into rows of fields.
///
/// A quoted field may hold newlines, so this walks the whole text rather than
/// splitting on lines first - which is the bug in every import that splits on
/// `\n` and then on the separator.
fn parse(text: &str, separator: char, quoted: bool, row_separator: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut inside = false;
    let mut characters = text.chars().peekable();
    let mut anything = false;
    while let Some(character) = characters.next() {
        if inside {
            if character == '"' {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                } else {
                    inside = false;
                }
            } else {
                field.push(character);
            }
            continue;
        }
        match character {
            '"' if quoted && field.is_empty() => {
                inside = true;
                anything = true;
            }
            _ if character == separator => {
                row.push(core::mem::take(&mut field));
                anything = true;
            }
            // A carriage return before a newline is the other half of a
            // Windows line ending and is not data. It only means that when the
            // newline is what ends a row: under `--rowsep` it is an ordinary
            // character, and dropping it would eat part of a field.
            '\r' if row_separator == '\n' => continue,
            _ if character == row_separator => {
                if anything || !field.is_empty() || !row.is_empty() {
                    row.push(core::mem::take(&mut field));
                    rows.push(core::mem::take(&mut row));
                }
                anything = false;
            }
            other => {
                field.push(other);
                anything = true;
            }
        }
    }
    if anything || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Returns an identifier quoted the way SQL wants it.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain fields split on the separator.
    #[test]
    fn fields_split_on_the_separator() {
        let rows = parse("a|b\nc|d\n", '|', false, '\n');
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    /// A quoted field may hold the separator and a newline.
    #[test]
    fn a_quoted_field_holds_anything() {
        let rows = parse("\"a,b\",c\n\"line\none\",d\n", ',', true, '\n');
        assert_eq!(rows, vec![vec!["a,b", "c"], vec!["line\none", "d"]]);
    }

    /// A doubled quote inside a quoted field is one quote.
    #[test]
    fn a_doubled_quote_is_one_quote() {
        let rows = parse("\"say \"\"hi\"\"\",x\n", ',', true, '\n');
        assert_eq!(rows, vec![vec!["say \"hi\"", "x"]]);
    }

    /// A last line with no newline is still a row.
    #[test]
    fn a_missing_final_newline_is_still_a_row() {
        let rows = parse("a|b", '|', false, '\n');
        assert_eq!(rows, vec![vec!["a", "b"]]);
    }

    /// `--ascii` reads the unit and record separators `.mode ascii` writes.
    ///
    /// A newline is ordinary text there, which is the point of the mode: a
    /// field may hold one without being quoted.
    #[test]
    fn ascii_separators_leave_a_newline_as_data() {
        let text = "a\u{1f}line\none\u{1e}c\u{1f}d\u{1e}";
        let rows = parse(text, UNIT_SEPARATOR, false, RECORD_SEPARATOR);
        assert_eq!(rows, vec![vec!["a", "line\none"], vec!["c", "d"]]);
    }
}
