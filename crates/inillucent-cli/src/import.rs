//! `.import`: reading a delimited file into a table.
//!
//! Invariant: a field is a string unless the table says otherwise. The import
//! does not guess types - it binds every field as text and lets the column's
//! affinity decide, which is exactly what an `INSERT` with a string literal
//! would do. Guessing here would make `.import` and `INSERT` disagree about the
//! same file, and the one that guessed would be the one that was wrong.
//!
//! The separator is whatever `.separator` is set to, and in `csv` mode the
//! quoting rules are CSV's: a field may be quoted, a doubled quote inside a
//! quoted field is one quote, and a separator inside quotes is data.

use crate::render::Mode;
use crate::shell::Shell;

/// Reads a file into a table.
pub fn import(shell: &mut Shell, arguments: &[&str]) {
    let (Some(path), Some(table)) = (arguments.first(), arguments.get(1)) else {
        shell.complain("Error: .import requires a file name and a table name");
        return;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        shell.complain(&format!("Error: cannot open \"{path}\""));
        return;
    };
    let separator = if shell.layout.mode == Mode::Csv {
        ','
    } else {
        shell.layout.separator.chars().next().unwrap_or('|')
    };
    let quoted = shell.layout.mode == Mode::Csv;
    let mut rows = parse(&text, separator, quoted);
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
    insert(shell, table, &rows, path);
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
fn parse(text: &str, separator: char, quoted: bool) -> Vec<Vec<String>> {
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
            '\r' => continue,
            '\n' => {
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
        let rows = parse("a|b\nc|d\n", '|', false);
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    /// A quoted field may hold the separator and a newline.
    #[test]
    fn a_quoted_field_holds_anything() {
        let rows = parse("\"a,b\",c\n\"line\none\",d\n", ',', true);
        assert_eq!(rows, vec![vec!["a,b", "c"], vec!["line\none", "d"]]);
    }

    /// A doubled quote inside a quoted field is one quote.
    #[test]
    fn a_doubled_quote_is_one_quote() {
        let rows = parse("\"say \"\"hi\"\"\",x\n", ',', true);
        assert_eq!(rows, vec![vec!["say \"hi\"", "x"]]);
    }

    /// A last line with no newline is still a row.
    #[test]
    fn a_missing_final_newline_is_still_a_row() {
        let rows = parse("a|b", '|', false);
        assert_eq!(rows, vec![vec!["a", "b"]]);
    }
}
