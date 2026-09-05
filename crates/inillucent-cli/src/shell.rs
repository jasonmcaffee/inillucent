//! The shell's state, and the loop that reads a line and decides what it is.
//!
//! Invariant: the shell is an adapter. It parses dot commands, formats output
//! and manages files; every statement it runs goes through the public `inillucent`
//! facade, and it never reaches past it. That is the rule that keeps the shell
//! from becoming a second, slightly different database - which is exactly what
//! happens to a shell that starts "just reading the schema directly".
//!
//! Input is accumulated until it is a complete statement. That is the one piece
//! of real logic here and it is not a nicety: a `CREATE TRIGGER` spans many
//! lines and holds semicolons inside its body, so "ends with a semicolon" is
//! wrong and the engine's own parser has to be the one that says when a
//! statement is finished.

use std::io::Write;

use inillucent::{Connection, Database, Value};

use crate::render::{render, Layout, Mode};

/// Everything the shell remembers between lines.
pub struct Shell {
    /// The database, kept boxed so the connection can borrow it.
    database: Box<Database>,
    /// The open connection.
    connection: Box<Connection>,
    /// Where the database came from, for `.databases` and the prompt.
    path: String,
    /// How results are laid out.
    pub layout: Layout,
    /// Where output goes, when it is not standard output.
    output: Option<std::fs::File>,
    /// Whether `.once` set that file for one statement only.
    output_is_once: bool,
    /// Whether a failing statement stops the script.
    pub bail: bool,
    /// Whether each statement is echoed before it runs.
    pub echo: bool,
    /// Whether to print how long each statement took.
    pub timer: bool,
    /// Whether to print the change count after each statement.
    pub show_changes: bool,
    /// Whether an `EXPLAIN QUERY PLAN` is printed before each statement.
    pub explain_plan: bool,
    /// Whether the shell should stop.
    pub done: bool,
    /// Whether anything has failed, which decides the exit code.
    pub failed: bool,
    /// The line the statement being run started on.
    pub line: usize,
}

/// Why a statement did not produce rows.
pub struct Failure {
    /// What went wrong.
    pub message: String,
    /// Where in the statement, when the failure knows.
    pub offset: Option<u32>,
    /// Whether it failed to compile rather than while running.
    pub compiling: bool,
}

impl Shell {
    /// Opens a shell on a database file, or on an in-memory one.
    pub fn open(path: &str) -> Result<Shell, String> {
        let database = Database::open(path).map_err(|error| error.message().to_string())?;
        let boxed = Box::new(database);
        // The connection borrows the database, which is boxed and never moves
        // while this shell is alive; the two are dropped together.
        let connection = boxed
            .connect()
            .map_err(|error| error.message().to_string())?;
        Ok(Shell {
            database: boxed,
            connection: Box::new(connection),
            path: path.to_string(),
            layout: Layout::default(),
            output: None,
            output_is_once: false,
            bail: false,
            echo: false,
            timer: false,
            show_changes: false,
            explain_plan: false,
            done: false,
            failed: false,
            line: 1,
        })
    }

    /// Returns the connection statements run on.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Returns where the database was opened from.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Closes the current database and opens another.
    pub fn reopen(&mut self, path: &str) -> Result<(), String> {
        let replacement = Shell::open(path)?;
        // The order matters: the old connection has to go before the database
        // it borrows, and replacing both fields at once is what does that.
        self.connection = replacement.connection;
        self.database = replacement.database;
        self.path = replacement.path;
        Ok(())
    }

    /// Prints one line to wherever output is currently going.
    pub fn say(&mut self, line: &str) {
        match self.output.as_mut() {
            Some(file) => {
                let _ = writeln!(file, "{line}");
            }
            None => println!("{line}"),
        }
    }

    /// Prints an error, which always goes to standard error.
    pub fn complain(&mut self, message: &str) {
        eprintln!("{message}");
        self.failed = true;
    }

    /// Sends output to a file, or back to standard output when `path` is none.
    pub fn redirect(&mut self, path: Option<&str>, once: bool) -> Result<(), String> {
        let Some(path) = path else {
            self.output = None;
            self.output_is_once = false;
            return Ok(());
        };
        let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
        self.output = Some(file);
        self.output_is_once = once;
        Ok(())
    }

    /// Returns output to the terminal after a `.once`.
    fn finish_once(&mut self) {
        if self.output_is_once {
            self.output = None;
            self.output_is_once = false;
        }
    }

    /// Runs one complete statement and prints whatever it produced.
    pub fn run(&mut self, sql: &str) {
        if self.echo {
            let text = sql.to_string();
            self.say(&text);
        }
        let started = std::time::Instant::now();
        if self.explain_plan {
            self.print_plan(sql);
        }
        let outcome = self.collect(sql);
        match outcome {
            Err(failure) => {
                self.report(sql, &failure);
            }
            Ok((columns, rows)) => {
                let layout = self.layout.clone();
                for line in render(&layout, &columns, &rows) {
                    self.say(&line);
                }
                if self.show_changes {
                    let changes = self.connection.changes();
                    self.say(&format!("changes: {changes}"));
                }
            }
        }
        if self.timer {
            let elapsed = started.elapsed();
            self.say(&format!("Run Time: real {:.3}", elapsed.as_secs_f64()));
        }
        self.finish_once();
    }

    /// Prints a failure the way the reference prints it.
    ///
    /// The caret block only appears when the failure knows where it happened,
    /// which is the same rule the reference follows: `no such table` has no
    /// position and `no such column` does.
    fn report(&mut self, sql: &str, failure: &Failure) {
        let line = self.line;
        let heading = if failure.compiling {
            format!("Parse error near line {line}: {}", failure.message)
        } else {
            format!("Error near line {line}: {}", failure.message)
        };
        self.complain(&heading);
        let Some(offset) = failure.offset.filter(|offset| *offset > 0) else {
            return;
        };
        let first = sql.lines().next().unwrap_or(sql);
        let column = (offset as usize).min(first.len());
        eprintln!("  {first}");
        eprintln!("  {}^--- error here", " ".repeat(column));
    }

    /// Runs a statement and collects its column names and rows.
    pub fn collect(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value<'static>>>), Failure> {
        let mut statement = self.connection.prepare(sql).map_err(|error| Failure {
            message: error.message().to_string(),
            offset: error.sql_offset(),
            compiling: true,
        })?;
        let columns: Vec<String> = statement
            .columns()
            .iter()
            .map(|column| String::from_utf8_lossy(&column.name).into_owned())
            .collect();
        let mut rows = Vec::new();
        loop {
            match statement.step() {
                Err(error) => {
                    return Err(Failure {
                        message: error.message().to_string(),
                        offset: None,
                        compiling: false,
                    })
                }
                Ok(false) => break,
                Ok(true) => rows.push(statement.row().to_vec()),
            }
        }
        Ok((columns, rows))
    }

    /// Prints the query plan for a statement, for `.eqp on`.
    fn print_plan(&mut self, sql: &str) {
        let plan = format!("EXPLAIN QUERY PLAN {sql}");
        let Ok((_, rows)) = self.collect(&plan) else {
            return;
        };
        for row in rows {
            let detail = row
                .last()
                .and_then(Value::as_text)
                .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
                .unwrap_or_default();
            self.say(&format!("QUERY PLAN {detail}"));
        }
    }

    /// Runs a statement for its effect, reporting only a failure.
    pub fn execute(&mut self, sql: &str) -> Result<(), String> {
        self.connection
            .execute_batch(sql)
            .map_err(|error| error.message().to_string())
    }

    /// Returns one column of one row, as text.
    pub fn scalar(&self, sql: &str) -> Option<String> {
        let (_, rows) = self.collect(sql).ok()?;
        let value = rows.first().and_then(|row| row.first())?;
        Some(match value {
            Value::Null => String::new(),
            Value::Text(text) => String::from_utf8_lossy(text.raw()).into_owned(),
            other => crate::render::literal(other),
        })
    }

    /// Returns the first column of every row, as text.
    pub fn column(&self, sql: &str) -> Vec<String> {
        let Ok((_, rows)) = self.collect(sql) else {
            return Vec::new();
        };
        rows.iter()
            .filter_map(|row| row.first())
            .map(|value| match value {
                Value::Null => String::new(),
                Value::Text(text) => String::from_utf8_lossy(text.raw()).into_owned(),
                other => crate::render::literal(other),
            })
            .collect()
    }
}

/// Reads input line by line, running statements as they become complete.
///
/// A line beginning with a dot is a command, but only when nothing is
/// half-typed: `.` inside a `CREATE TRIGGER` body is part of the statement, and
/// treating it as a command there is the bug every naive shell has.
pub fn drive(shell: &mut Shell, input: impl Iterator<Item = String>) {
    let mut pending = String::new();
    let mut number = 0usize;
    let mut started = 1usize;
    for line in input {
        number += 1;
        if pending.trim().is_empty() {
            started = number;
        }
        shell.line = started;
        if pending.trim().is_empty() && line.trim_start().starts_with('.') {
            if shell.echo {
                let text = line.trim().to_string();
                shell.say(&text);
            }
            crate::dot::run(shell, line.trim());
            if shell.done || (shell.failed && shell.bail) {
                return;
            }
            continue;
        }
        pending.push_str(&line);
        pending.push('\n');
        while let Some(consumed) = complete_statement(shell, &pending) {
            let statement = pending.get(..consumed).unwrap_or_default().to_string();
            let rest = pending.split_off(consumed);
            pending = rest;
            if !statement.trim().is_empty() {
                shell.run(statement.trim());
                if shell.done || (shell.failed && shell.bail) {
                    return;
                }
            }
        }
    }
    if !pending.trim().is_empty() {
        // Whatever is left was never terminated. Running it is what SQLite's
        // shell does at end of input, and it is what makes `echo "SELECT 1" |
        // inillucent-shell` work without a semicolon.
        let statement = pending.trim().to_string();
        shell.run(&statement);
    }
}

/// Returns how many bytes of `text` form one complete statement, if any.
///
/// This is `sqlite3_complete`, and it is lexical on purpose. Asking the parser
/// cannot work: a `CREATE TRIGGER` does not parse until its `END`, and a parse
/// failure does not distinguish "still typing" from "misspelt". What a shell
/// needs to know is narrower and decidable - has a semicolon been reached that
/// is not inside a trigger body - so that is what is computed.
fn complete_statement(_shell: &Shell, text: &str) -> Option<usize> {
    let mut state = State::Start;
    let bytes = text.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        let Some(byte) = bytes.get(at).copied() else {
            break;
        };
        match byte {
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                at = skip_line_comment(bytes, at);
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let Some(next) = skip_block_comment(bytes, at) else {
                    // An unterminated block comment is more input to come.
                    return None;
                };
                at = next;
            }
            b'\'' | b'"' | b'`' => {
                let Some(next) = skip_quoted(bytes, at, byte) else {
                    return None;
                };
                at = next;
            }
            b'[' => {
                let Some(next) = skip_quoted(bytes, at, b']') else {
                    return None;
                };
                at = next;
            }
            b';' => {
                at += 1;
                if state.ends_here() {
                    return Some(at);
                }
                state = state.after_semicolon();
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' => {
                let end = word_end(bytes, at);
                let word = bytes.get(at..end).unwrap_or(&[]).to_ascii_uppercase();
                state = state.after_word(&word);
                at = end;
            }
            _ if byte.is_ascii_whitespace() => at += 1,
            _ => {
                state = state.after_other();
                at += 1;
            }
        }
    }
    None
}

/// Where the scan is, in terms of what a semicolon would mean.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Nothing has been read yet, or the last statement finished.
    Start,
    /// A statement is under way and a semicolon ends it.
    Plain,
    /// `CREATE` has been read, and the next words decide.
    Create,
    /// `CREATE ... TRIGGER` has been read; the body has not started.
    Trigger,
    /// Inside a trigger body, where a semicolon ends a nested statement.
    Body,
    /// `END` has been read inside a body, so a semicolon ends the whole thing.
    End,
}

impl State {
    /// Returns whether a semicolon here finishes the statement.
    fn ends_here(self) -> bool {
        !matches!(self, State::Trigger | State::Body)
    }

    /// Returns the state after a semicolon that did not finish anything.
    fn after_semicolon(self) -> State {
        match self {
            State::Trigger | State::Body => State::Body,
            _ => State::Start,
        }
    }

    /// Returns the state after a word.
    fn after_word(self, word: &[u8]) -> State {
        match self {
            State::Start if word == b"CREATE" => State::Create,
            State::Start if word == b"EXPLAIN" => State::Start,
            State::Start => State::Plain,
            // `TEMP`, `TEMPORARY` and `IF NOT EXISTS` all sit between `CREATE`
            // and the thing being created, so they leave the state alone.
            State::Create
                if matches!(
                    word,
                    b"TEMP" | b"TEMPORARY" | b"IF" | b"NOT" | b"EXISTS" | b"OR" | b"REPLACE"
                ) =>
            {
                State::Create
            }
            State::Create if word == b"TRIGGER" => State::Trigger,
            State::Create => State::Plain,
            State::Trigger if word == b"BEGIN" => State::Body,
            State::Body if word == b"END" => State::End,
            State::End => State::Body,
            other => other,
        }
    }

    /// Returns the state after anything that is not a word or a semicolon.
    fn after_other(self) -> State {
        match self {
            State::Start => State::Plain,
            State::End => State::Body,
            other => other,
        }
    }
}

/// Returns the offset just past a `--` comment.
fn skip_line_comment(bytes: &[u8], at: usize) -> usize {
    let mut scan = at + 2;
    while scan < bytes.len() {
        if bytes.get(scan) == Some(&b'\n') {
            return scan + 1;
        }
        scan += 1;
    }
    scan
}

/// Returns the offset just past a block comment, or `None` when it is open.
fn skip_block_comment(bytes: &[u8], at: usize) -> Option<usize> {
    let mut scan = at + 2;
    while scan + 1 < bytes.len() {
        if bytes.get(scan) == Some(&b'*') && bytes.get(scan + 1) == Some(&b'/') {
            return Some(scan + 2);
        }
        scan += 1;
    }
    None
}

/// Returns the offset just past a quoted run, or `None` when it is open.
///
/// A doubled quote inside a quoted run is one character and does not close it,
/// which is the case a naive scan gets wrong on `'it''s'`.
fn skip_quoted(bytes: &[u8], at: usize, close: u8) -> Option<usize> {
    let open = bytes.get(at).copied()?;
    let mut scan = at + 1;
    while scan < bytes.len() {
        let byte = bytes.get(scan).copied()?;
        if byte == close {
            if close == open && bytes.get(scan + 1) == Some(&close) {
                scan += 2;
                continue;
            }
            return Some(scan + 1);
        }
        scan += 1;
    }
    None
}

/// Returns the offset just past a word.
fn word_end(bytes: &[u8], at: usize) -> usize {
    let mut scan = at;
    while scan < bytes.len() {
        match bytes.get(scan) {
            Some(byte) if byte.is_ascii_alphanumeric() || *byte == b'_' => scan += 1,
            _ => break,
        }
    }
    scan
}

/// Returns the mode a `.mode` argument selects, or a message.
pub fn mode_named(name: &str) -> Result<Mode, String> {
    Mode::from_name(name).ok_or_else(|| format!("Error: mode should be one of: {}", MODE_NAMES))
}

/// Every mode name, for the message above and for `.help`.
pub const MODE_NAMES: &str = "box column csv html insert json line list markdown quote table tabs";
