//! `.dump`: the SQL that would rebuild the database.
//!
//! Invariant: what comes out reproduces what went in, and it says so at the
//! top. A dump is wrapped in a transaction and begins with
//! `PRAGMA foreign_keys=OFF`, because the rows come out in `sqlite_master`
//! order rather than in dependency order and a foreign key would refuse a child
//! whose parent is three statements later. That is SQLite's dump format, and a
//! dump that could not be fed back in is not a dump.

use inillucent_value::Value;

use crate::render::literal;
use crate::shell::Shell;

/// Writes the whole database, or one table, as SQL.
pub fn dump(shell: &mut Shell, pattern: Option<&str>) {
    shell.say("PRAGMA foreign_keys=OFF;");
    shell.say("BEGIN TRANSACTION;");
    let objects = schema_objects(shell, pattern);
    // Tables first, with their rows, then everything that refers to them: an
    // index or a trigger on a table that does not exist yet is an error, and
    // this ordering is what stops the dump from producing one.
    for (kind, name, sql) in &objects {
        if kind != "table" {
            continue;
        }
        say_definition(shell, name, sql);
        write_rows(shell, name);
    }
    for (kind, _, sql) in &objects {
        if kind == "table" {
            continue;
        }
        shell.say(&format!("{sql};"));
    }
    shell.say("COMMIT;");
}

/// Returns the schema rows a dump covers, in creation order.
fn schema_objects(shell: &Shell, pattern: Option<&str>) -> Vec<(String, String, String)> {
    let mut sql = String::from(
        "SELECT type, name, sql FROM sqlite_master \
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'",
    );
    if let Some(pattern) = pattern {
        let quoted = format!("'{}'", pattern.replace('\'', "''"));
        sql.push_str(&format!(
            " AND (name LIKE {quoted} OR tbl_name LIKE {quoted})"
        ));
    }
    // Tables first - the loop below relies on it - and then the rest by type
    // descending, which is view, trigger, index. That order is not cosmetic: a
    // trigger may name a view, so the view has to be created first.
    sql.push_str(" ORDER BY type = 'table' DESC, type DESC, rowid");
    let Ok((_, rows)) = shell.collect(&sql) else {
        return Vec::new();
    };
    rows.iter()
        .map(|row| (plain(row.first()), plain(row.get(1)), plain(row.get(2))))
        .collect()
}

/// Returns a value as plain text.
fn plain(value: Option<&Value<'static>>) -> String {
    match value {
        Some(Value::Text(text)) => String::from_utf8_lossy(text.raw()).into_owned(),
        Some(Value::Null) | None => String::new(),
        Some(other) => literal(other),
    }
}

/// Writes one object's definition, keeping a virtual table's own form.
fn say_definition(shell: &mut Shell, name: &str, sql: &str) {
    let _ = name;
    shell.say(&format!("{sql};"));
}

/// Writes every row of one table as an `INSERT`.
fn write_rows(shell: &mut Shell, table: &str) {
    let quoted = quote_identifier(table);
    let Ok((_, rows)) = shell.collect(&format!("SELECT * FROM {quoted}")) else {
        return;
    };
    for row in rows {
        let values: Vec<String> = row.iter().map(literal).collect();
        shell.say(&format!(
            "INSERT INTO {quoted} VALUES({});",
            values.join(",")
        ));
    }
}

/// Returns an identifier, quoted only when it has to be.
///
/// A dump is read by a person as often as by a program, and quoting every name
/// makes it noisier than the schema it came from. The rule is the usual one: a
/// name that is a bare word needs nothing.
fn quote_identifier(name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !name.starts_with(|character: char| character.is_ascii_digit());
    if plain {
        return name.to_string();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare word is left alone and anything else is quoted, with a quote
    /// inside it doubled rather than dropped.
    #[test]
    fn an_identifier_is_quoted_only_when_it_has_to_be() {
        assert_eq!(quote_identifier("t"), "t");
        assert_eq!(quote_identifier("with_underscore"), "with_underscore");
        assert_eq!(quote_identifier("has space"), "\"has space\"");
        assert_eq!(quote_identifier("1leading"), "\"1leading\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
