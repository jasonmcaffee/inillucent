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

use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::Value;

use crate::render::{render, Layout, Mode};

/// Everything the shell remembers between lines.
pub struct Shell {
    /// The database.
    ///
    /// A connection is a borrow of it rather than a thing of its own, so one is
    /// made where it is used instead of being stored - storing it beside the
    /// database it borrows would be a self-referential struct for no gain.
    database: Database,
    /// The session every one of those borrows is a continuation of.
    ///
    /// **Because a shell is one connection, not one per statement.** Temporary
    /// objects belong to a session: `CREATE TEMP TABLE t(a)` puts `t` in the
    /// session's own database, and a `SELECT` on a *different* session cannot
    /// see it. Calling `Database::connect` per statement opened a new session
    /// each time, so the shell reported success on the `CREATE` and then
    /// `no such table: t` on the very next line (task-1843).
    ///
    /// task-1844 fixed exactly this shape inside the engine and added
    /// `connect_as` for callers that hand out a connection per call over one
    /// logical connection; the shell is one of those and was not converted.
    session: u64,
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
    /// Where `.log` was pointed, when it was pointed anywhere.
    ///
    /// Recorded and never written to: this engine emits no log messages, so the
    /// destination is a place nothing arrives. `.show` reports it, which is the
    /// only thing that reads it.
    pub log_to: Option<String>,
    /// The values `.parameter set` bound, by the name they were given.
    ///
    /// **The shell's own table, not the engine's.** SQLite keeps them in a
    /// `temp.sqlite_parameters` table and binds from it before each step; the
    /// visible behaviour is the same and this needs no reserved table name.
    /// Ordered by key, which is the order `.parameter list` prints and the
    /// order the reference prints.
    pub parameters: std::collections::BTreeMap<String, Value<'static>>,
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
        let session = database.connect().session();
        Ok(Shell {
            database,
            session,
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
            log_to: None,
            parameters: std::collections::BTreeMap::new(),
            line: 1,
        })
    }

    /// Returns the connection statements run on.
    ///
    /// Always the same session, so a temporary object made by one statement is
    /// there for the next one.
    pub fn connection(&self) -> Connection<'_> {
        self.database.connect_as(self.session)
    }

    /// Copies the open database into a file and checks the copy.
    ///
    /// @param path - where the copy goes
    pub fn backup_to(&self, path: &str) -> Result<(), String> {
        self.database
            .backup_to(path)
            .map_err(|error| error.message().to_string())
    }

    /// Returns where the database was opened from.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Closes the current database and opens another.
    pub fn reopen(&mut self, path: &str) -> Result<(), String> {
        let replacement = Shell::open(path)?;
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
                // **`EXPLAIN QUERY PLAN` is drawn, not listed.** Its four
                // columns are a tree, and the reference's shell renders them as
                // one; printing `0|0|0|SCAN t` is the raw result of a statement
                // nobody writes for the raw result.
                if is_query_plan(sql) {
                    for line in plan_tree(&rows) {
                        self.say(&line);
                    }
                    self.finish_once();
                    return;
                }
                let layout = self.layout.clone();
                for line in render(&layout, &columns, &rows) {
                    self.say(&line);
                }
                if self.show_changes {
                    let changes = self.connection().changes();
                    // The reference prints both counters, aligned with three
                    // spaces between them.
                    let total = self.connection().total_changes();
                    self.say(&format!("changes: {changes}   total_changes: {total}"));
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
        let Some(offset) = failure.offset else {
            return;
        };
        for line in error_context(sql.as_bytes(), offset as usize) {
            self.complain(&line);
        }
    }

    /// Runs a statement and collects its column names and rows.
    pub fn collect(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Value<'static>>>), Failure> {
        let connection = self.connection();
        let mut statement = connection.prepare(sql).map_err(|error| Failure {
            message: reason(&error),
            offset: error.sql_offset(),
            compiling: true,
        })?;
        // **What `.parameter set` bound, applied by name.** A statement that
        // names none of them binds nothing; a name the statement does not use
        // is not an error, which is what makes a set of parameters reusable
        // across a script.
        if !self.parameters.is_empty() {
            let names = connection.parameter_names(sql).unwrap_or_default();
            for (name, index) in names {
                let key = String::from_utf8_lossy(&name).into_owned();
                let Some(value) = self.parameters.get(&key) else {
                    continue;
                };
                let _ = statement.bind(index, crate::shell::datum_of(value));
            }
        }
        let mut rows = Vec::new();
        loop {
            match statement.step() {
                Err(error) => {
                    return Err(Failure {
                        message: reason(&error),
                        offset: None,
                        compiling: false,
                    })
                }
                Ok(false) => break,
                Ok(true) => rows.push(statement.row().iter().map(value_of).collect()),
            }
        }
        // Read *after* stepping. The engine's statement materialises on its
        // first step, so it does not know its column names until it has run -
        // where `sqlite3_column_name` answers straight after a prepare. Asking
        // first returned an empty list, and `.headers on` printed nothing.
        let columns: Vec<String> = statement.columns().to_vec();
        Ok((columns, rows))
    }

    /// Prints the query plan for a statement, for `.eqp on`.
    fn print_plan(&mut self, sql: &str) {
        let plan = format!("EXPLAIN QUERY PLAN {sql}");
        let Ok((_, rows)) = self.collect(&plan) else {
            return;
        };
        for line in plan_tree(&rows) {
            self.say(&line);
        }
    }

    /// Runs a statement for its effect, reporting only a failure.
    pub fn execute(&mut self, sql: &str) -> Result<(), String> {
        self.connection()
            .execute_batch(sql)
            .map_err(|error| reason(&error))
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

/// Returns a datum as the value the renderer formats.
///
/// The engine's rows are `OwnedDatum` and everything that prints one takes
/// `Value`, which is `inillucent-value`'s type and the one the affinity and
/// collation rules are written against. Converting here rather than rewriting
/// `render.rs` keeps the formatting - `.mode`, `.nullvalue`, the width
/// calculation - exactly as it was, which is what a caller of this shell would
/// notice if it changed.
///
/// @param datum - one value out of a row
fn value_of(datum: &OwnedDatum) -> Value<'static> {
    match datum {
        OwnedDatum::Null => Value::Null,
        OwnedDatum::Int(number) => Value::Integer(*number),
        OwnedDatum::Real(number) => Value::Real(*number),
        OwnedDatum::Text(bytes) => Value::owned_text(bytes).unwrap_or(Value::Null),
        OwnedDatum::Blob(bytes) => Value::owned_blob(bytes).unwrap_or(Value::Null),
    }
}

/// Returns one value as the datum a bind takes.
///
/// The reverse of [`value_of`], and the shell's own half of `.parameter`.
///
/// @param value - the value the shell is holding
pub fn datum_of(value: &Value<'static>) -> OwnedDatum {
    match value {
        Value::Null => OwnedDatum::Null,
        Value::Integer(number) => OwnedDatum::Int(*number),
        Value::Real(number) => OwnedDatum::Real(*number),
        Value::Text(text) => OwnedDatum::Text(text.raw().to_vec()),
        Value::Blob(blob) => OwnedDatum::Blob(blob.raw().to_vec()),
    }
}

/// Reports whether a statement is an `EXPLAIN QUERY PLAN`.
///
/// The words rather than the bound statement, because the shell decides how to
/// *print* before it knows what the engine made of it - and the two spellings
/// SQLite accepts are `EXPLAIN QUERY PLAN` and nothing else.
///
/// @param sql - the statement as typed
fn is_query_plan(sql: &str) -> bool {
    let mut words = sql.split_whitespace();
    words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("explain"))
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("query"))
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("plan"))
}

/// Renders `EXPLAIN QUERY PLAN`'s four columns as the tree the reference draws.
///
/// **The rows are a tree and were being printed as rows.** Each carries an id
/// and its parent's id, and the reference draws them under a `QUERY PLAN`
/// heading, with `|--` for a node that has a sibling after it and a backtick
/// arm for the last, indented three characters per level - which is how a
/// subquery under a step is told from a step beside it. Printing the raw four
/// columns left the shape for the reader to work out.
///
/// @param rows - the plan's rows: id, parent, notused, detail
pub fn plan_tree(rows: &[Vec<Value<'static>>]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["QUERY PLAN".to_string()];
    plan_children(rows, 0, "", 0, &mut lines);
    lines
}

/// The arm the reference draws under the last child of a node.
const LAST_ARM: &str = "`--";

/// How deep a plan tree may be drawn before the walk gives up.
///
/// A plan that named itself as its own parent would otherwise not terminate,
/// and a malformed plan is not a reason for a shell to hang. The cap rather
/// than an id check, because this engine numbers its top-level rows from zero
/// and the root is asked for by parent zero - so a row whose id and parent are
/// both zero is the ordinary first line of every plan.
const PLAN_DEPTH: usize = 64;

/// Emits one parent's children, and theirs.
///
/// @param rows - every row of the plan
/// @param parent - the id whose children to emit
/// @param prefix - the indent the ancestors give
/// @param depth - how deep this call is
/// @param lines - where the rendered lines go
fn plan_children(
    rows: &[Vec<Value<'static>>],
    parent: i64,
    prefix: &str,
    depth: usize,
    lines: &mut Vec<String>,
) {
    if depth >= PLAN_DEPTH {
        return;
    }
    let field = |row: &Vec<Value<'static>>, at: usize| -> i64 {
        row.get(at).and_then(Value::as_integer).unwrap_or(0)
    };
    let children: Vec<&Vec<Value<'static>>> =
        rows.iter().filter(|row| field(row, 1) == parent).collect();
    for (at, row) in children.iter().enumerate() {
        let last = at.saturating_add(1) == children.len();
        let detail = row
            .last()
            .and_then(Value::as_text)
            .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
            .unwrap_or_default();
        let arm = if last { LAST_ARM } else { "|--" };
        lines.push(format!("{prefix}{arm}{detail}"));
        // A node that still has siblings below it keeps a vertical bar in its
        // children's indent; the last one leaves a space.
        let carried = format!("{prefix}{}", if last { "   " } else { "|  " });
        // A row that names its own parent's id is its own child, which is what
        // a top-level row looks like on an engine that numbers from zero: it
        // has id 0 and parent 0. It is selected as a child of the root and must
        // not then be expanded as its own parent.
        if field(row, 0) != parent {
            plan_children(
                rows,
                field(row, 0),
                &carried,
                depth.saturating_add(1),
                lines,
            );
        }
    }
}

/// Returns the two lines that point at where a statement went wrong.
///
/// A port of the reference shell's `shell_error_context`, down to the two
/// arrangements of the marker and the number that chooses between them, because
/// this is one of the places a transcript is compared rather than read. The
/// reference slides a window along the statement so the offending token is never
/// off the left of the line, truncates at 78 bytes, flattens every space
/// character to a plain space so a tab cannot shift the marker, and then draws
/// the caret to the left of the token while it still fits and to the right of a
/// trailing rule once it does not.
///
/// Returns nothing when the position is not inside the statement, which is the
/// reference's answer for `no such table` and for everything that fails while
/// stepping rather than while parsing.
///
/// @param sql - the whole statement, as the shell was given it
/// @param offset - the byte the engine says the error is at
fn error_context(sql: &[u8], offset: usize) -> Vec<String> {
    if offset >= sql.len() {
        return Vec::new();
    }
    // Slide the window right until the marker is within 50 bytes of the start,
    // never stopping inside a UTF-8 sequence.
    let mut start = 0usize;
    let mut column = offset;
    while column > 50 {
        start += 1;
        column -= 1;
        while sql.get(start).is_some_and(|byte| byte & 0xc0 == 0x80) {
            start += 1;
            column -= 1;
        }
    }
    let window = sql.get(start..).unwrap_or_default();
    let mut length = window.len().min(78);
    while length > 0 && window.get(length).is_some_and(|byte| byte & 0xc0 == 0x80) {
        length -= 1;
    }
    let shown = String::from_utf8_lossy(window.get(..length).unwrap_or_default())
        .chars()
        .map(|character| {
            if character.is_ascii_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let marker = if column < 25 {
        format!("  {}^--- error here", " ".repeat(column))
    } else {
        format!("  {}error here ---^", " ".repeat(column - 14))
    };
    vec![format!("  {shown}"), marker]
}

/// Returns what a failure should say to a person.
///
/// **The detail, when there is one, and the code's text otherwise.** A
/// `DbError`'s `message` is the text of its primary code - "bad parameter or
/// other API misuse" for everything the engine refuses - and the sentence a
/// person can act on is in `detail`: "no such table: nope". Printing the code's
/// text made every refusal look like the same failure, which is the opposite of
/// what a shell is for.
///
/// @param error - what went wrong
fn reason(error: &inillucent_base::DbError) -> String {
    error
        .detail()
        .unwrap_or_else(|| error.message())
        .to_string()
}
