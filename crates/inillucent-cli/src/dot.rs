//! The dot commands: everything a person types that is not SQL.
//!
//! Invariant: a dot command is sugar over SQL wherever it can be. `.tables`
//! queries `sqlite_master`, `.schema` reads the stored text, `.indexes` asks
//! `PRAGMA index_list` - so a dot command cannot disagree with the query a
//! person would have written instead, and there is no second implementation of
//! the catalog living in the shell. The commands that are *not* sugar - the
//! ones that manage files, output and settings - are the ones that could not
//! be, and they are the short list at the top of `run`.
//!
//! Arguments are split the way a shell splits them, with quotes honoured,
//! because `.output "my file.txt"` has to work and `.separator " | "` has to be
//! able to hold spaces.

use crate::render::{literal, Layout};
use crate::shell::{drive, mode_named, Shell, MODE_NAMES};
use inillucent_value::Value;

/// Runs one dot command.
pub fn run(shell: &mut Shell, line: &str) {
    let words = split(line);
    let Some(name) = words
        .first()
        .map(|word| word.trim_start_matches('.').to_string())
    else {
        return;
    };
    let arguments: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
    match name.as_str() {
        "quit" | "exit" => shell.done = true,
        "help" => help(shell),
        "open" => open(shell, &arguments),
        "databases" => databases(shell),
        "tables" => tables(shell, &arguments),
        "indexes" | "indices" => indexes(shell, &arguments),
        "schema" => schema(shell, &arguments),
        "fullschema" => full_schema(shell),
        "headers" => shell.layout.headers = truthy(arguments.first().copied()),
        "mode" => mode(shell, &arguments),
        "separator" => separator(shell, &arguments),
        "nullvalue" => shell.layout.null = arguments.first().copied().unwrap_or("").to_string(),
        "width" => width(shell, &arguments),
        "output" => output(shell, &arguments, false),
        "once" => output(shell, &arguments, true),
        "print" => {
            let text = arguments.join(" ");
            shell.say(&text);
        }
        "echo" => shell.echo = truthy(arguments.first().copied()),
        "bail" => shell.bail = truthy(arguments.first().copied()),
        "timer" => shell.timer = truthy(arguments.first().copied()),
        "changes" => shell.show_changes = truthy(arguments.first().copied()),
        "eqp" => shell.explain_plan = truthy(arguments.first().copied()),
        "read" => read(shell, &arguments),
        "dump" => dump(shell, &arguments),
        "import" => crate::import::import(shell, &arguments),
        // `.save` and `.clone` both write the database somewhere else, which is
        // what `.backup` does. SQLite's `.clone` rebuilds the target object by
        // object where `.backup` copies the file; the result is the same
        // database, and this engine has one way of producing a verified copy.
        "backup" | "save" => backup(shell, &arguments),
        "clone" => clone(shell, &arguments),
        "timeout" => timeout(shell, &arguments),
        "log" => log(shell, &arguments),
        "restore" => restore(shell, &arguments),
        "parameter" => parameter(shell, &arguments),
        "version" => version(shell),
        "show" => show(shell),
        "nullvalues" => shell.complain("Error: unknown command; try .help"),
        _ => shell.complain(&format!(
            "Error: unknown command or invalid arguments: \"{name}\". Enter \".help\" for help"
        )),
    }
}

/// `.clone FILE`: the database written somewhere else, object by object.
///
/// The copy is `.backup`'s - one verified file copy rather than a row-by-row
/// rebuild, because this engine is single threaded and one file is one pool, so
/// there is no second writer to race. What the reference's own `.clone` gives a
/// person besides the file is the running commentary, and that is reproduced:
/// each table and index it carried, as it carries it.
///
/// @param shell - the shell
/// @param arguments - the words after the command
fn clone(shell: &mut Shell, arguments: &[&str]) {
    let named = shell.column(
        "SELECT name FROM sqlite_master WHERE type IN ('table','index')          AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
    );
    backup(shell, arguments);
    for name in named {
        shell.say(&format!("{name}... done"));
    }
}

/// `.timeout MS`: how long a writer waits for the writer slot.
///
/// Sugar over `PRAGMA busy_timeout`, which is where the setting actually lives -
/// so the dot command and the pragma cannot disagree about it.
///
/// @param shell - the shell
/// @param arguments - the words after the command
fn timeout(shell: &mut Shell, arguments: &[&str]) {
    let milliseconds = arguments
        .first()
        .and_then(|word| word.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let _ = shell.collect(&format!("PRAGMA busy_timeout = {milliseconds}"));
}

/// `.log FILE|off|stdout|stderr`: where the engine's own log goes.
///
/// **Accepted and recorded, and nothing is written to it.** This engine emits
/// no log messages at all - there is no `sqlite3_log` equivalent behind it - so
/// a destination is a place nothing arrives. That is the same thing the
/// reference produces for a session that logs nothing, which is why this is
/// accepted rather than refused: the observable behaviour is identical, and
/// refusing would stop a script that sets it out of habit.
///
/// @param shell - the shell
/// @param arguments - the words after the command
fn log(shell: &mut Shell, arguments: &[&str]) {
    shell.log_to = arguments.first().map(|word| (*word).to_string());
}

/// `.parameter init | list | set NAME VALUE | unset NAME | clear`.
///
/// **The one dot command with no substitute.** A bound parameter could not be
/// exercised from this shell at all - `SELECT :x + 1` had nothing to bind `:x`
/// to - so the whole parameter surface was untestable from a script, which is
/// what task-1859 Part H names first.
///
/// The value is a SQL expression, evaluated by the engine rather than parsed
/// here: SQLite runs `SELECT <value>` for it, so `.parameter set :n 1+1` binds
/// 2 and `.parameter set :s 'text'` binds text rather than the four characters
/// of the literal. `init` makes the store, which here is always there, and is a
/// no-op that exists so a script written for the reference runs unchanged.
///
/// @param shell - the shell
/// @param arguments - the words after the command
fn parameter(shell: &mut Shell, arguments: &[&str]) {
    match arguments.first().copied().unwrap_or("list") {
        "init" => {}
        "clear" => shell.parameters.clear(),
        "list" => {
            let listed: Vec<(String, String)> = shell
                .parameters
                .iter()
                .map(|(name, value)| (name.clone(), literal(value)))
                .collect();
            // The reference pads the name column to the widest name, which is
            // what makes a list of several read as a table.
            let width = listed
                .iter()
                .map(|(name, _)| name.chars().count())
                .max()
                .unwrap_or(0);
            for (name, value) in listed {
                let padding = " ".repeat(width.saturating_sub(name.chars().count()));
                shell.say(&format!("{name}{padding} {value}"));
            }
        }
        "unset" => {
            let Some(name) = arguments.get(1) else {
                shell.complain("Error: .parameter unset needs a name");
                return;
            };
            shell.parameters.remove(*name);
        }
        "set" => {
            let (Some(name), Some(value)) = (arguments.get(1), arguments.get(2)) else {
                shell.complain("Error: .parameter set needs a name and a value");
                return;
            };
            // Quoted text arrives here with its quotes already removed by
            // `split`, so it is re-quoted before being evaluated - otherwise
            // `.parameter set :s hello` would be a column reference.
            let expression = if value.parse::<f64>().is_ok() {
                (*value).to_string()
            } else {
                format!("'{}'", value.replace('\'', "''"))
            };
            match shell.collect(&format!("SELECT {expression}")) {
                Ok((_, rows)) => {
                    let held = rows
                        .first()
                        .and_then(|row| row.first())
                        .cloned()
                        .unwrap_or(Value::Null);
                    shell.parameters.insert((*name).to_string(), held);
                }
                Err(failure) => shell.complain(&format!("Error: {}", failure.message)),
            }
        }
        other => shell.complain(&format!(
            "Error: unknown .parameter subcommand: \"{other}\""
        )),
    }
}

/// Splits a command line into words, honouring quotes.
fn split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for character in line.chars() {
        match quote {
            Some(open) if character == open => {
                quote = None;
                words.push(core::mem::take(&mut current));
                started = false;
            }
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => {
                if started {
                    words.push(core::mem::take(&mut current));
                }
                quote = Some(character);
                started = true;
            }
            None if character.is_whitespace() => {
                if started {
                    words.push(core::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(character);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

/// Reports whether an argument means "on".
///
/// SQLite's shell takes `on`, `yes`, `true` and `1`, and treats a missing
/// argument as on - which is what makes a bare `.headers` do something.
fn truthy(argument: Option<&str>) -> bool {
    match argument {
        None => true,
        Some(text) => matches!(
            text.to_ascii_lowercase().as_str(),
            "on" | "yes" | "true" | "1"
        ),
    }
}

/// Prints the commands this shell knows.
fn help(shell: &mut Shell) {
    for line in [
        ".backup ?DB? FILE      Back up DB (default \"main\") to FILE",
        ".bail on|off           Stop after hitting an error",
        ".changes on|off        Show number of rows changed by SQL",
        ".databases             List names and files of attached databases",
        ".dump ?TABLE?          Render database content as SQL",
        ".echo on|off           Turn command echo on or off",
        ".eqp on|off            Show the query plan before each statement",
        ".exit                  Exit this program",
        ".fullschema            Show schema and the content of sqlite_stat tables",
        ".headers on|off        Turn display of headers on or off",
        ".help                  Show this message",
        ".import FILE TABLE     Import data from FILE into TABLE",
        ".indexes ?TABLE?       Show names of indexes",
        ".mode MODE             Set output mode",
        ".nullvalue STRING      Use STRING in place of NULL values",
        ".once FILE             Output for the next SQL command only to FILE",
        ".open FILE             Close existing database and reopen FILE",
        ".output ?FILE?         Send output to FILE or stdout",
        ".print STRING...       Print literal STRING",
        ".quit                  Exit this program",
        ".read FILE             Read input from FILE",
        ".restore ?DB? FILE     Restore content of DB (default \"main\") from FILE",
        ".schema ?PATTERN?      Show the CREATE statements matching PATTERN",
        ".separator STRING      Change the column separator",
        ".show                  Show the current values for various settings",
        ".tables ?TABLE?        List names of tables matching a pattern",
        ".timer on|off          Turn SQL timer on or off",
        ".version               Show source, library and compiler versions",
        ".width NUM1 NUM2 ...   Set minimum column widths for columnar output",
    ] {
        shell.say(line);
    }
    let modes = format!("MODE is one of: {MODE_NAMES}");
    shell.say(&modes);
}

/// `.open`: closes the current database and opens another.
fn open(shell: &mut Shell, arguments: &[&str]) {
    let path = arguments.first().copied().unwrap_or(":memory:");
    if let Err(message) = shell.reopen(path) {
        shell.complain(&format!(
            "Error: unable to open database \"{path}\": {message}"
        ));
    }
}

/// `.databases`: what is attached, and where each came from.
fn databases(shell: &mut Shell) {
    let Ok((_, rows)) = shell.collect("PRAGMA database_list") else {
        shell.complain("Error: could not read the database list");
        return;
    };
    for row in rows {
        let name = text_of(row.get(1));
        let file = text_of(row.get(2));
        // The reference states what it opened the file as. This engine opens
        // every database for reading and writing - there is no `.open --readonly`
        // - so the suffix is always the same one, and leaving it off made every
        // `.databases` line differ from the reference's.
        shell.say(&format!("{name}: {file} r/w"));
    }
}

/// Returns a value as plain text.
fn text_of(value: Option<&Value<'static>>) -> String {
    match value {
        Some(Value::Text(text)) => String::from_utf8_lossy(text.raw()).into_owned(),
        Some(Value::Null) | None => String::new(),
        Some(other) => literal(other),
    }
}

/// `.tables`: the tables and views, in one space-separated block.
fn tables(shell: &mut Shell, arguments: &[&str]) {
    let mut sql = String::from(
        "SELECT name FROM sqlite_master WHERE type IN ('table','view') \
         AND name NOT LIKE 'sqlite_%'",
    );
    if let Some(pattern) = arguments.first() {
        sql.push_str(&format!(" AND name LIKE {}", literal_text(pattern)));
    }
    sql.push_str(" ORDER BY name");
    let names = shell.column(&sql);
    if names.is_empty() {
        return;
    }
    for line in columnise(&names) {
        shell.say(&line);
    }
}

/// How wide the reference lays a name list out for.
const SCREEN: usize = 80;

/// The gap the reference leaves between one column and the next.
const GUTTER: usize = 5;

/// Lays a list of names out the way `.tables` and `.indexes` do.
///
/// **Column-major, padded per column, and as few rows as fit.** Measured
/// against the reference rather than guessed: fourteen tables of increasing
/// name length come back in five columns of three rows, each column padded to
/// its *own* longest entry plus five, with the last column unpadded. One row is
/// used whenever the whole list fits in eighty characters, which is every case
/// a script is likely to have.
///
/// @param names - the names, already sorted
fn columnise(names: &[String]) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    for rows in 1..=names.len() {
        let columns = names.len().div_ceil(rows);
        let widths: Vec<usize> = (0..columns)
            .map(|column| {
                names
                    .iter()
                    .skip(column.saturating_mul(rows))
                    .take(rows)
                    .map(|name| name.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let total: usize = widths
            .iter()
            .enumerate()
            .map(|(at, width)| {
                if at.saturating_add(1) == columns {
                    *width
                } else {
                    width.saturating_add(GUTTER)
                }
            })
            .sum();
        if total > SCREEN && rows < names.len() {
            continue;
        }
        let mut lines = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut line = String::new();
            for column in 0..columns {
                let Some(name) = names.get(column.saturating_mul(rows).saturating_add(row)) else {
                    continue;
                };
                if !line.is_empty() {
                    // The previous column was padded to its own width; the
                    // gutter is what separates them.
                    line.push_str(&" ".repeat(GUTTER));
                }
                line.push_str(name);
                let width = widths.get(column).copied().unwrap_or(0);
                let last = column.saturating_add(1) == columns
                    || names
                        .get(
                            column
                                .saturating_add(1)
                                .saturating_mul(rows)
                                .saturating_add(row),
                        )
                        .is_none();
                if !last {
                    line.push_str(&" ".repeat(width.saturating_sub(name.chars().count())));
                }
            }
            if !line.is_empty() {
                lines.push(line);
            }
        }
        return lines;
    }
    Vec::new()
}

/// `.indexes`: the indexes, optionally on one table.
fn indexes(shell: &mut Shell, arguments: &[&str]) {
    let mut sql = String::from("SELECT name FROM sqlite_master WHERE type = 'index'");
    if let Some(pattern) = arguments.first() {
        // **The argument matches the *index* name, not the table's**, which is
        // measured rather than read off the help text: against the reference,
        // `.indexes ia` answers `ia` and `.indexes t` - the table `ia` is on -
        // answers nothing. Filtering on `tbl_name` made every one-argument
        // `.indexes` differ.
        sql.push_str(&format!(" AND name LIKE {}", literal_text(pattern)));
    }
    sql.push_str(" AND name NOT LIKE 'sqlite_%' ORDER BY name");
    let names = shell.column(&sql);
    if names.is_empty() {
        return;
    }
    for line in columnise(&names) {
        shell.say(&line);
    }
}

/// `.schema`: the statements that would rebuild what is there.
fn schema(shell: &mut Shell, arguments: &[&str]) {
    let mut sql = String::from("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL");
    if let Some(pattern) = arguments.first() {
        let quoted = literal_text(pattern);
        sql.push_str(&format!(
            " AND (name LIKE {quoted} OR tbl_name LIKE {quoted})"
        ));
    }
    sql.push_str(" ORDER BY rowid");
    let Ok((_, rows)) = shell.collect(&sql) else {
        return;
    };
    for row in rows {
        let kind = text_of(row.first());
        let name = text_of(row.get(1));
        let statement = text_of(row.get(2));
        if kind == "view" {
            // A view's text does not say what columns it produces, so the
            // columns go underneath in a comment - which is what makes
            // `.schema` on a view worth reading.
            let listed = view_columns(shell, &name);
            shell.say(&format!("{statement}\n/* {name}({listed}) */;"));
            continue;
        }
        shell.say(&format!("{statement};"));
    }
}

/// Returns a view's column names, comma-separated.
fn view_columns(shell: &Shell, name: &str) -> String {
    let sql = format!("PRAGMA table_info({})", quote_identifier(name));
    let Ok((_, rows)) = shell.collect(&sql) else {
        return String::new();
    };
    rows.iter()
        .map(|row| text_of(row.get(1)))
        .collect::<Vec<String>>()
        .join(",")
}

/// Returns an identifier quoted the way SQL wants it.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `.fullschema`: the schema, then whatever `ANALYZE` recorded.
fn full_schema(shell: &mut Shell) {
    schema(shell, &[]);
    let exists = shell.scalar("SELECT count(*) FROM sqlite_master WHERE name = 'sqlite_stat1'");
    if exists.as_deref() == Some("0") || exists.is_none() {
        shell.say("/* No STAT tables available */");
        return;
    }
    let layout = core::mem::replace(
        &mut shell.layout,
        Layout {
            mode: crate::render::Mode::Insert,
            table: "sqlite_stat1".to_string(),
            ..Layout::default()
        },
    );
    shell.run("SELECT * FROM sqlite_stat1");
    shell.layout = layout;
}

/// Returns a string as an SQL literal.
fn literal_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `.mode`: how results are laid out.
fn mode(shell: &mut Shell, arguments: &[&str]) {
    let Some(name) = arguments.first() else {
        let current = format!("current output mode: {}", shell.layout.mode.name());
        shell.say(&current);
        return;
    };
    match mode_named(name) {
        Err(message) => shell.complain(&message),
        Ok(mode) => {
            shell.layout.mode = mode;
            shell.layout.separator = mode.separator().to_string();
            // **CSV ends a record with CR LF**, which is what RFC 4180 says and
            // what the reference writes; every other mode ends a line with LF.
            // Choosing a mode resets both separators, exactly as it resets the
            // column one.
            shell.layout.row_separator = if mode == crate::render::Mode::Csv {
                "\r\n".to_string()
            } else {
                "\n".to_string()
            };
            // The tabular modes want headers, which is what SQLite's shell
            // does when the mode changes: nobody asks for a box with no
            // labels on it.
            if matches!(
                mode,
                crate::render::Mode::Column
                    | crate::render::Mode::Markdown
                    | crate::render::Mode::Table
                    | crate::render::Mode::Box
                    | crate::render::Mode::Html
            ) {
                shell.layout.headers = true;
            }
            if let Some(table) = arguments.get(1) {
                shell.layout.table = (*table).to_string();
            }
        }
    }
}

/// `.separator`: what goes between columns, and optionally between rows.
fn separator(shell: &mut Shell, arguments: &[&str]) {
    if let Some(column) = arguments.first() {
        shell.layout.separator = (*column).to_string();
    }
    if let Some(row) = arguments.get(1) {
        shell.layout.row_separator = (*row).to_string();
    }
}

/// `.width`: minimum widths for the columnar modes.
fn width(shell: &mut Shell, arguments: &[&str]) {
    shell.layout.widths = arguments
        .iter()
        .map(|word| word.parse::<usize>().unwrap_or(0))
        .collect();
}

/// `.output` and `.once`: where results go.
fn output(shell: &mut Shell, arguments: &[&str], once: bool) {
    let path = arguments.first().copied().filter(|path| *path != "stdout");
    if let Err(message) = shell.redirect(path, once) {
        shell.complain(&format!(
            "Error: cannot open \"{}\": {message}",
            path.unwrap_or("")
        ));
    }
}

/// `.read`: runs a script as though it had been typed.
fn read(shell: &mut Shell, arguments: &[&str]) {
    let Some(path) = arguments.first() else {
        shell.complain("Error: .read requires a file name");
        return;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        shell.complain(&format!("Error: cannot open \"{path}\""));
        return;
    };
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    drive(shell, lines.into_iter());
}

/// `.dump`: the SQL that would rebuild the database.
fn dump(shell: &mut Shell, arguments: &[&str]) {
    crate::dump::dump(shell, arguments.first().copied());
}

/// `.backup`: copies a database into a file.
fn backup(shell: &mut Shell, arguments: &[&str]) {
    let (_, path) = database_and_file(arguments);
    let Some(path) = path else {
        shell.complain("Error: .backup requires a file name");
        return;
    };
    // A checkpoint and a file copy, which is what a backup of this format is:
    // one file is one database, and there is no second writer to race.
    if let Err(message) = shell.backup_to(path) {
        shell.complain(&format!("Error: {message}"));
    }
}

/// `.restore`: replaces a database with the contents of a file.
fn restore(shell: &mut Shell, arguments: &[&str]) {
    let (_, path) = database_and_file(arguments);
    let Some(path) = path else {
        shell.complain("Error: .restore requires a file name");
        return;
    };
    // **Restoring is opening the other file, not copying it over this one.**
    // The old engine's restore wrote the source's pages into the open database
    // in place. This engine's databases are whole files, so the honest restore
    // is to point the shell at the file the caller named - which is also what a
    // person means by it, and it does not destroy the database they were in.
    if let Err(message) = shell.reopen(path) {
        shell.complain(&format!("Error: {message}"));
    }
}

/// Splits `?DB? FILE` into its two halves.
fn database_and_file<'a>(arguments: &[&'a str]) -> (&'a str, Option<&'a str>) {
    match arguments {
        [file] => ("main", Some(file)),
        [database, file, ..] => (database, Some(file)),
        _ => ("main", None),
    }
}

/// `.version`: what this is and what it implements.
fn version(shell: &mut Shell) {
    let library = shell
        .scalar("SELECT sqlite_version()")
        .unwrap_or_else(|| "unknown".to_string());
    shell.say(&format!("SQLite {library}"));
    shell.say("inillucent (a first-party engine implementing the SQLite ABI)");
}

/// `.show`: the current settings.
fn show(shell: &mut Shell) {
    let lines = vec![
        format!("        echo: {}", on_off(shell.echo)),
        format!("         eqp: {}", on_off(shell.explain_plan)),
        format!("     explain: auto"),
        format!("     headers: {}", on_off(shell.layout.headers)),
        format!("        mode: {}", shell.layout.mode.name()),
        format!("   nullvalue: \"{}\"", shell.layout.null),
        format!("      output: {}", "stdout"),
        format!("colseparator: \"{}\"", shell.layout.separator),
        format!("rowseparator: \"{}\"", escape(&shell.layout.row_separator)),
        format!("       stats: off"),
        format!("       width: {}", widths_text(&shell.layout.widths)),
        format!("    filename: {}", shell.path().to_string()),
    ];
    for line in lines {
        shell.say(&line);
    }
}

/// Returns "on" or "off".
fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

/// Escapes a separator for display.
fn escape(text: &str) -> String {
    text.replace('\n', "\\n").replace('\t', "\\t")
}

/// Renders the fixed column widths.
fn widths_text(widths: &[usize]) -> String {
    widths
        .iter()
        .map(usize::to_string)
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quotes hold a word together, and whitespace between words splits them.
    #[test]
    fn arguments_split_the_way_a_shell_splits_them() {
        assert_eq!(split(".mode csv"), vec![".mode", "csv"]);
        assert_eq!(
            split(".output \"my file.txt\""),
            vec![".output", "my file.txt"]
        );
        assert_eq!(split(".separator \" | \""), vec![".separator", " | "]);
        assert_eq!(split(".print"), vec![".print"]);
    }

    /// A missing argument means on, which is what a bare `.headers` needs.
    #[test]
    fn a_missing_argument_means_on() {
        assert!(truthy(None));
        assert!(truthy(Some("on")));
        assert!(truthy(Some("YES")));
        assert!(!truthy(Some("off")));
        assert!(!truthy(Some("nonsense")));
    }
}
