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
/// One open database and the session statements on it belong to.
pub struct Opened {
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
    /// `no such table: t` on the very next line.
    ///
    /// The engine was fixed to add `connect_as` for callers that hand out a
    /// connection per call over one logical connection; the shell is one of
    /// those and was not converted.
    session: u64,
    /// Where the database came from, for `.databases` and the prompt.
    path: String,
}

/// How many databases `.connection` can hold open at once.
///
/// Five, which is the reference's own array size. A slot that has never been
/// switched to is closed, and switching to one opens an in-memory database
/// there - which is what makes `.connection 1` a working command on a shell
/// that was started with one file.
pub const CONNECTIONS: usize = 5;

/// One shell session: the databases it can reach, and every setting a dot
/// command can change.
pub struct Shell {
    /// The databases `.connection` switches between; slot 0 is the one the
    /// shell was started on.
    connections: Vec<Option<Opened>>,
    /// Which slot statements run on.
    active: usize,
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
    /// Whether the page cache's counters are printed after each statement.
    pub stats: bool,
    /// Whether to print the change count after each statement.
    pub show_changes: bool,
    /// Whether an `EXPLAIN QUERY PLAN` is printed before each statement.
    pub explain_plan: bool,
    /// Whether output lines end with a carriage return, which `.crlf` sets.
    pub crlf: bool,
    /// The prompt an interactive session prints for a new statement.
    pub prompt_main: String,
    /// The prompt it prints for a statement that is not finished.
    pub prompt_continue: String,
    /// When an `EXPLAIN` listing is laid out as a table.
    pub explain_mode: crate::commands::ExplainMode,
    /// The token `.nonce` set, which suspends safe mode for one command.
    pub nonce: Option<String>,
    /// The name of the `.testcase` that is capturing output, if one is.
    pub testcase: Option<String>,
    /// What has been printed since that `.testcase`.
    pub captured: String,
    /// How many `.check`s have run.
    pub tests_run: usize,
    /// How many of them failed.
    pub tests_failed: usize,
    /// Where a `.excel` or `.www` file is being written, if one is.
    pub viewer: Option<std::path::PathBuf>,
    /// Whether the authorizer's decisions are printed, which `.auth` sets.
    pub auth: bool,
    /// The decisions it has recorded since the last statement.
    pub authorized: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    /// Where `.trace` sends each statement, when it sends it anywhere.
    pub trace: Option<String>,
    /// What `.scanstats` was set to.
    pub scanstats: String,
    /// Whether `SQLITE_DBCONFIG_DEFENSIVE` is in force.
    ///
    /// **On, because the reference's shell turns it on.** It is the flag that
    /// makes `PRAGMA journal_mode = OFF` and `PRAGMA writable_schema = ON`
    /// refuse rather than take effect, and a shell that left it off answered
    /// those two differently from the reference on a fresh database.
    pub defensive: bool,
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
    /// How often `.progress` was asked to run a handler, in opcodes.
    ///
    /// Recorded and reported by `.show`, and never acted on: the reference's
    /// handler prints nothing unless `--limit` is given, and this engine's VM
    /// has no per-opcode callback to hang one on. Keeping the state means a
    /// script written for the reference sets it and runs on rather than
    /// stopping at "unknown command".
    pub progress_interval: u64,
    /// The `--limit` `.progress` was given.
    pub progress_limit: u64,
    /// Whether `.progress --once` was asked for.
    pub progress_once: bool,
    /// Whether `.progress --quiet` was asked for.
    pub progress_quiet: bool,
    /// Whether the next number belongs to a `--limit` that has just been read.
    pub progress_pending_limit: bool,
    /// Whether a statement that changes something is refused.
    ///
    /// `-readonly` on the command line, and `--readonly` on `inillucent` and
    /// `inillucent-mcp`. **The binder decides what writes, not a scan of the
    /// text**: `EXPLAIN QUERY PLAN` over the statement fails with "not a
    /// read-only statement" for anything that does, which cannot be talked past
    /// with whitespace, a comment or an unusual capitalisation. It is the same
    /// mechanism `inillucent-driver` uses and it is deliberately the same one -
    /// two classifiers would eventually disagree, and the one that let a write
    /// through would be the one nobody was watching.
    ///
    /// The *file* is still open for writing. The capability table says
    /// `readonly_open` is `partial` and says exactly this, which is why the row
    /// is worth reading before an application decides what it means by "open
    /// this read only".
    pub readonly: bool,
    /// Whether the commands that reach outside the database are refused.
    ///
    /// `-safe` on the command line. The set is the reference's: running a
    /// program (`.shell`, `.system`), loading a shared library (`.load`),
    /// changing the working directory (`.cd`), handing a file to whatever the
    /// system opens it with (`.excel`, `.www`), and writing output through a
    /// pipe (`.output |cmd`, `.once |cmd`). Every one of them is a way for a
    /// script that was only supposed to query a database to run code.
    ///
    /// `.nonce` lifts it for one command, which is the reference's own escape
    /// hatch and is why the token is a secret the script's author chose.
    pub safe: bool,
    /// Where output goes when a caller is collecting it rather than printing.
    ///
    /// **Not the same as `captured`, and deliberately outside it.** `captured`
    /// belongs to `.testcase`/`.check`, which compare one command's output
    /// against an expected digest; this belongs to a caller running the shell
    /// as a subroutine - the `run` command and the MCP server behind it
    /// - and has to still be collecting while a `.testcase` inside the script it
    /// was given is doing its own thing. So `say` checks the testcase first and
    /// this second, and a script that uses both nests the way it reads.
    ///
    /// `complain` writes here too, because a caller collecting output wants the
    /// error in the same stream a person would have seen it in. It still sets
    /// `failed`.
    pub sink: Option<String>,
    /// The values `.parameter set` bound, by the name they were given.
    ///
    /// **The shell's own table, not the engine's.** SQLite keeps them in a
    /// `temp.sqlite_parameters` table and binds from it before each step; the
    /// visible behaviour is the same and this needs no reserved table name.
    /// Ordered by key, which is the order `.parameter list` prints and the
    /// order the reference prints.
    pub parameters: std::collections::BTreeMap<String, Value<'static>>,
}

/// Returns an error as a sentence, with its detail when it carries one.
///
/// @param error - the failure
fn described(error: inillucent_base::DbError) -> String {
    match error.detail() {
        Some(detail) => format!("{}: {detail}", error.message()),
        None => error.message().to_string(),
    }
}

/// Why a statement did not produce rows.
pub struct Failure {
    /// What went wrong.
    pub message: String,
    /// Where in the statement, when the failure knows.
    pub offset: Option<u32>,
    /// Whether it failed to compile rather than while running.
    pub compiling: bool,
    /// The engine's own error, kept so a caller can classify it.
    ///
    /// **The message is not the classification.** The command layer
    /// has to tell a caller whether a statement was refused because the engine
    /// has not built the construct - the driver's `unsupported` - or because it
    /// was mistyped, and `drivers/README.md` argues at length for why folding
    /// those two together throws the design away. Only the `DbError` knows:
    /// `unsupported()` is a field on it, and the primary code separates a
    /// constraint from a busy file from corruption. Rendering it to a sentence
    /// here and matching on the sentence there would be a second, worse
    /// classifier beside the driver's.
    pub error: Option<inillucent_base::DbError>,
}

impl Shell {
    /// Opens one database, with the modules and the flags a shell gives it.
    ///
    /// @param path - the file, or an in-memory name
    pub fn open_one(path: &str) -> Result<Opened, String> {
        // **The detail, not only the code.** An open that fails with "bad
        // parameter or other API misuse" and nothing else is an error nobody
        // can act on; the detail says which part of the file could not be read.
        let database = Database::open(path).map_err(described)?;
        // **The shell adds `fsdir`, and the library does not.** A table-valued
        // function over the file system belongs to a program that asked for
        // one; the reference draws the same line, with `fsdir` in `shell.c`.
        for module in [
            std::sync::Arc::new(inillucent_engine::ext::vtab::fsdir::FsDirModule)
                as std::sync::Arc<dyn inillucent_engine::ext::vtab::Module>,
            std::sync::Arc::new(inillucent_engine::ext::vtab::zipfile::ZipFileModule),
        ] {
            database
                .register_module(module)
                .map_err(|error| error.message().to_string())?;
        }
        let session = database.connect().session();
        // **The reference's shell turns this on and this one has to as well.**
        // It is a connection flag rather than a shell one, so setting the field
        // below is not enough: the engine has to be told, or
        // `PRAGMA journal_mode = OFF` is honoured here and refused there.
        database.connect_as(session).set_defensive(true);
        Ok(Opened {
            database,
            session,
            path: path.to_string(),
        })
    }

    /// Opens a shell on a database file, or on an in-memory one.
    pub fn open(path: &str) -> Result<Shell, String> {
        let mut connections: Vec<Option<Opened>> = (0..CONNECTIONS).map(|_| None).collect();
        if let Some(first) = connections.first_mut() {
            *first = Some(Shell::open_one(path)?);
        }
        Ok(Shell {
            connections,
            active: 0,
            layout: Layout::default(),
            output: None,
            output_is_once: false,
            bail: false,
            echo: false,
            timer: false,
            stats: false,
            show_changes: false,
            explain_plan: false,
            crlf: false,
            prompt_main: "sqlite> ".to_string(),
            prompt_continue: "   ...> ".to_string(),
            explain_mode: crate::commands::ExplainMode::Auto,
            nonce: None,
            testcase: None,
            captured: String::new(),
            tests_run: 0,
            tests_failed: 0,
            viewer: None,
            auth: false,
            authorized: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            trace: None,
            scanstats: "off".to_string(),
            defensive: true,
            done: false,
            failed: false,
            log_to: None,
            progress_interval: 0,
            progress_limit: 0,
            progress_once: false,
            progress_quiet: false,
            progress_pending_limit: false,
            parameters: std::collections::BTreeMap::new(),
            readonly: false,
            safe: false,
            sink: None,
            line: 1,
        })
    }

    /// Returns the connection statements run on.
    ///
    /// Always the same session, so a temporary object made by one statement is
    /// there for the next one.
    pub fn connection(&self) -> Connection<'_> {
        let held = self.open_slot();
        held.database.connect_as(held.session)
    }

    /// Returns whether a boolean pragma reads on.
    ///
    /// @param name - the pragma's name
    pub fn boolean_pragma(&self, name: &str) -> bool {
        self.column(&format!("PRAGMA {name};"))
            .first()
            .is_some_and(|value| value == "1")
    }

    /// Sets a boolean pragma, reporting whether the engine took it.
    ///
    /// @param name - the pragma's name
    /// @param value - what to set it to
    pub fn set_boolean_pragma(&mut self, name: &str, value: bool) -> bool {
        let word = if value { "on" } else { "off" };
        self.collect(&format!("PRAGMA {name} = {word};")).is_ok()
            && self.boolean_pragma(name) == value
    }

    /// Installs or removes the authorizer that `.auth on` prints through.
    ///
    /// @param on - whether the decisions are watched
    pub fn set_authorizer(&mut self, on: bool) {
        let installed: Option<std::rc::Rc<dyn inillucent_engine::Authorizer>> = on.then(|| {
            std::rc::Rc::new(crate::commands::Watching {
                seen: std::rc::Rc::clone(&self.authorized),
            }) as std::rc::Rc<dyn inillucent_engine::Authorizer>
        });
        self.connection().set_authorizer(installed);
    }

    /// Prints and clears whatever the authorizer recorded.
    fn report_authorized(&mut self) {
        let lines: Vec<String> = self.authorized.borrow_mut().drain(..).collect();
        for line in lines {
            self.say(&line);
        }
    }

    /// Puts the connection into or out of defensive mode.
    ///
    /// @param on - whether the flag is in force
    pub fn set_defensive(&mut self, on: bool) -> bool {
        self.connection().set_defensive(on);
        true
    }

    /// Returns what the page cache has been asked to do.
    pub fn cache_stats(&self) -> inillucent_engine::connect::CacheStats {
        self.open_slot().database.cache_stats()
    }

    /// Returns how many bytes the page cache is holding.
    pub fn pool_bytes(&self) -> usize {
        self.open_slot().database.pool_bytes()
    }

    /// Copies the open database into a file and checks the copy.
    ///
    /// @param path - where the copy goes
    pub fn backup_to(&self, path: &str) -> Result<(), String> {
        self.open_slot()
            .database
            .backup_to(path)
            .map_err(|error| error.message().to_string())
    }

    /// Returns where the database was opened from.
    pub fn path(&self) -> &str {
        &self.open_slot().path
    }

    /// Returns the database statements currently run on.
    ///
    /// The active slot is never closed: `.connection close` on it moves the
    /// shell back to slot zero, and slot zero is opened before the shell is.
    ///
    /// **The `expect` is the invariant, and the invariant is enforced twice.**
    /// `Shell::open` fills slot zero before the shell exists, and
    /// `.connection close` on the active slot moves back to slot zero rather
    /// than closing it. The crate denies `expect_used` because a shell that
    /// panics on a caller's SQL is unusable; this is not that - reaching it
    /// would mean the shell had been constructed without a database, which no
    /// path does.
    #[allow(clippy::expect_used)]
    fn open_slot(&self) -> &Opened {
        self.connections
            .get(self.active)
            .and_then(|held| held.as_ref())
            .or_else(|| self.connections.first().and_then(|held| held.as_ref()))
            .expect("the shell always holds one open database")
    }

    /// Returns which slot statements run on.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Returns each slot and what it holds, for `.connection`.
    pub fn slots(&self) -> Vec<Option<String>> {
        self.connections
            .iter()
            .map(|held| held.as_ref().map(|open| open.path.clone()))
            .collect()
    }

    /// Switches to one slot, opening an in-memory database if it is closed.
    ///
    /// Out of range is ignored, which is what the reference does with it.
    ///
    /// @param slot - which connection to run statements on
    pub fn use_slot(&mut self, slot: usize) -> Result<(), String> {
        if slot >= CONNECTIONS {
            return Ok(());
        }
        if self.connections.get(slot).is_some_and(Option::is_none) {
            let opened = Shell::open_one(":memory:")?;
            if let Some(place) = self.connections.get_mut(slot) {
                *place = Some(opened);
            }
        }
        self.active = slot;
        Ok(())
    }

    /// Closes one slot, moving back to slot zero if it was the active one.
    ///
    /// Slot zero is never closed: it is the database the shell was started on,
    /// and a shell with nothing open has nothing to run a statement against.
    ///
    /// @param slot - which connection to close
    pub fn close_slot(&mut self, slot: usize) {
        if slot == 0 || slot >= CONNECTIONS {
            return;
        }
        if let Some(place) = self.connections.get_mut(slot) {
            *place = None;
        }
        if self.active == slot {
            self.active = 0;
        }
    }

    /// Closes the current database and opens another.
    pub fn reopen(&mut self, path: &str) -> Result<(), String> {
        let replacement = Shell::open_one(path)?;
        let active = self.active;
        if let Some(place) = self.connections.get_mut(active) {
            *place = Some(replacement);
        }
        Ok(())
    }

    /// Prints one line to wherever output is currently going.
    pub fn say(&mut self, line: &str) {
        // **A `.testcase` captures instead of printing.** `.check` compares the
        // output of the commands between the two, and a passing case prints
        // nothing at all - which is what makes a test script's output the list
        // of the cases that failed.
        if self.testcase.is_some() {
            self.captured.push_str(line);
            self.captured.push('\n');
            return;
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.push_str(line);
            sink.push('\n');
            return;
        }
        let ending = if self.crlf { "\r\n" } else { "\n" };
        match self.output.as_mut() {
            Some(file) => {
                let _ = write!(file, "{line}{ending}");
            }
            None => {
                let mut out = std::io::stdout();
                let _ = write!(out, "{line}{ending}");
            }
        }
    }

    /// Prints an error, which always goes to standard error.
    ///
    /// Unless a caller is collecting output, in which case it goes there: a
    /// command run through the MCP server has no standard error anybody will
    /// ever read, and an error that vanished would be worse than one printed
    /// among the rows.
    pub fn complain(&mut self, message: &str) {
        match self.sink.as_mut() {
            Some(sink) => {
                sink.push_str(message);
                sink.push('\n');
            }
            None => eprintln!("{message}"),
        }
        self.failed = true;
    }

    /// Refuses a command that safe mode does not allow, and says which it was.
    ///
    /// Returns whether the caller may go on. A matching `.nonce` has already
    /// cleared safe mode for this command by the time this is asked, because
    /// that is what `.nonce` does.
    ///
    /// @param command - the dot command being attempted, leading dot included
    pub fn unsafe_refused(&mut self, command: &str) -> bool {
        if !self.safe {
            return false;
        }
        self.complain(&format!("Error: {command} is prohibited in safe mode"));
        true
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
        // `.excel` and `.www` hand the file to whatever the system opens that
        // kind with, and only once it is closed and complete.
        if let Some(path) = self.viewer.take() {
            crate::commands::open_viewer(&path);
        }
    }

    /// Runs one complete statement and prints whatever it produced.
    pub fn run(&mut self, sql: &str) {
        if self.readonly && self.writes(sql) {
            self.complain("Error: attempt to write a readonly database");
            return;
        }
        if self.echo {
            let text = sql.to_string();
            self.say(&text);
        }
        let started = std::time::Instant::now();
        if self.explain_plan {
            self.print_plan(sql);
        }
        let outcome = self.collect(sql);
        // `.auth on` prints what the binder asked about, before the rows the
        // statement produced - which is the order the reference prints them in.
        if self.auth {
            self.report_authorized();
        }
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
                // **And the bytecode form is a table with fixed columns.** The
                // reference's shell switches to its own `MODE_Explain` for an
                // `EXPLAIN` whatever `.mode` says, because eight columns of
                // opcode printed as `0|Init|0|1|0||0|Start at 1` is unreadable.
                // The widths are the reference's own.
                let as_table = match self.explain_mode {
                    crate::commands::ExplainMode::Auto => is_bytecode_explain(sql),
                    crate::commands::ExplainMode::On => true,
                    crate::commands::ExplainMode::Off => false,
                };
                if as_table && columns.len() == EXPLAIN_WIDTHS.len() {
                    for line in explain_table(&columns, &rows) {
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
        if self.stats {
            for line in crate::diagnose::statistics(self) {
                self.say(&line);
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
        self.collect_bound(sql, &[])
    }

    /// Runs a statement with values bound by position, and collects its rows.
    ///
    /// **By position, because a caller that is a program has no names.** The
    /// shell's own `.parameter` table binds `:name` and `@name` markers, which
    /// is what a person typing a script wants; a command arriving over MCP or
    /// off a command line carries an ordered array and means `?1`, `?2`, ... .
    /// Going through the named table for those was the first thing tried,
    /// and it bound nothing at all: the engine reports a numbered marker
    /// under a name that is not the text `?1`, so every lookup missed and every
    /// value silently arrived as NULL. Binding by the index the parser assigned
    /// cannot miss.
    ///
    /// Both mechanisms apply: positional values are bound first and the named
    /// table after, so a script that sets `:limit` once and passes `?1` per
    /// call gets both.
    ///
    /// @param sql - the statement
    /// @param bound - the values for `?1`, `?2`, ... in order
    pub fn collect_bound(
        &self,
        sql: &str,
        bound: &[OwnedDatum],
    ) -> Result<(Vec<String>, Vec<Vec<Value<'static>>>), Failure> {
        let connection = self.connection();
        let mut statement = connection.prepare(sql).map_err(|error| Failure {
            message: reason(&error),
            offset: error.sql_offset(),
            compiling: true,
            error: Some(error),
        })?;
        for (nth, value) in bound.iter().enumerate() {
            // The parser numbers markers from one, and a caller that passed
            // more values than the statement has markers is told so rather than
            // having the extras dropped: a query that silently ignored an
            // argument is a query answering a different question.
            statement
                .bind(nth as u32 + 1, value.clone())
                .map_err(|error| Failure {
                    message: reason(&error),
                    offset: None,
                    compiling: true,
                    error: Some(error),
                })?;
        }
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
                        error: Some(error),
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

    /// Returns the text left over after the first statement, when it holds another one.
    ///
    /// **The parser's own count, not a scan for semicolons.** A trigger body contains a semicolon,
    /// and a string literal can contain anything, so counting them is how a correct script gets
    /// refused and an incorrect one gets accepted. `prepare_with_tail` reports how many bytes the
    /// first statement used, and `leading_trivia` reports how much of what is left is not a
    /// statement at all - which is what makes a trailing semicolon and a trailing comment not count
    /// as a second statement. Both are the engine's own, so there is no second scanner here to
    /// disagree with the parser.
    ///
    /// A script that will not compile answers `None`: it is a syntax error, and it should be
    /// reported as the syntax error it is rather than as a script with too many statements in it.
    ///
    /// @param sql - the text a caller passed as one statement
    pub fn trailing_statement(&self, sql: &str) -> Option<String> {
        let connection = self.connection();
        let (_, consumed) = connection.prepare_with_tail(sql).ok()?;
        let left = sql.get(consumed..)?;
        let rest = left
            .get(inillucent_engine::connect::leading_trivia(left)..)?
            .trim();
        if rest.is_empty() {
            return None;
        }
        Some(rest.chars().take(60).collect())
    }

    /// Runs a statement for its effect, reporting only a failure.
    ///
    /// @param sql - the statements, separated by semicolons
    pub fn execute(&mut self, sql: &str) -> Result<(), String> {
        self.connection()
            .execute_batch(sql)
            .map_err(|error| reason(&error))
    }

    /// Returns whether a statement changes something, as the binder sees it.
    ///
    /// A statement that fails to plan for any other reason - a missing table, a
    /// construct the engine has not built - answers `false`, so it reaches the
    /// ordinary path and is reported as the failure it actually is. Telling a
    /// caller to reopen the file over a typo would be worse than not having the
    /// flag.
    ///
    /// @param sql - the statement
    pub fn writes(&self, sql: &str) -> bool {
        match self.connection().explain(sql) {
            Ok(_) => false,
            Err(error) => error.message().contains("not a read-only statement"),
        }
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
                // `?` rather than a `let ... else`: an unterminated block
                // comment is more input to come, which is what `None` means all
                // the way up this function.
                at = skip_block_comment(bytes, at)?;
            }
            b'\'' | b'"' | b'`' => {
                at = skip_quoted(bytes, at, byte)?;
            }
            b'[' => {
                at = skip_quoted(bytes, at, b']')?;
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

/// The column widths the reference prints an `EXPLAIN` listing in.
///
/// `addr`, `opcode`, `p1`, `p2`, `p3`, `p4`, `p5`, `comment` - the same numbers
/// its shell carries, so a listing lines up under the same headings.
const EXPLAIN_WIDTHS: [usize; 8] = [4, 13, 4, 4, 4, 13, 2, 13];

/// Reports whether a statement is an `EXPLAIN` in its bytecode form.
///
/// The word `EXPLAIN` not followed by `QUERY`, which is the only other thing it
/// can be followed by.
///
/// @param sql - the statement as typed
fn is_bytecode_explain(sql: &str) -> bool {
    let mut words = sql.split_whitespace();
    words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("explain"))
        && !words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("query"))
}

/// Renders an `EXPLAIN` listing as the reference's fixed-width table.
///
/// A header, a rule of dashes, then one line per instruction, each column
/// left-aligned in its own width and separated by two spaces. A value wider
/// than its column is not truncated - the reference does not truncate either,
/// and a clipped opcode name would be worse than a ragged line.
///
/// @param columns - the column names, which are the reference's headings
/// @param rows - the instructions
fn explain_table(columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    /// What separates two columns.
    const GAP: &str = "  ";

    let mut lines = Vec::with_capacity(rows.len().saturating_add(2));
    lines.push(
        columns
            .iter()
            .enumerate()
            .map(|(at, name)| pad(name, EXPLAIN_WIDTHS.get(at).copied().unwrap_or(0)))
            .collect::<Vec<String>>()
            .join(GAP),
    );
    lines.push(
        EXPLAIN_WIDTHS
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<String>>()
            .join(GAP),
    );
    let last = EXPLAIN_WIDTHS.len().saturating_sub(1);
    for row in rows {
        let cells: Vec<String> = (0..EXPLAIN_WIDTHS.len())
            .map(|at| {
                let text = match row.get(at) {
                    Some(Value::Text(text)) => String::from_utf8_lossy(text.raw()).into_owned(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => crate::render::literal(other),
                };
                // **The last column of a row is written as it is.** The heading
                // is padded and the instruction's comment is not, which is what
                // leaves a `Halt` line ending in the separator rather than in
                // thirteen spaces. It is a small thing and it is two bytes of
                // difference per line against the reference.
                if at == last {
                    text
                } else {
                    pad(&text, EXPLAIN_WIDTHS.get(at).copied().unwrap_or(0))
                }
            })
            .collect();
        lines.push(cells.join(GAP));
    }
    lines
}

/// Left-aligns one cell in its column.
///
/// @param text - the cell
/// @param width - the column's width
fn pad(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
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

#[cfg(test)]
mod trailing_statement_tests {
    use super::Shell;

    /// Opens a scratch shell over a database that reaches no file.
    fn shell() -> Shell {
        Shell::open(":memory:").expect("a memory database opens")
    }

    /// One statement is one statement, however it is punctuated.
    ///
    /// `exec` and `query` are documented as taking one, and used to run the first of several and
    /// report success - which is how `inillucent exec "<twenty CREATE TABLEs>"` produced a database
    /// with one table in it and printed `ok. 0 rows changed.` These are the cases the
    /// refusal must not fire on, and the one it must.
    #[test]
    fn a_second_statement_is_recognised_and_punctuation_is_not() {
        let held = shell();
        for one in [
            "CREATE TABLE a (id INTEGER PRIMARY KEY)",
            "CREATE TABLE a (id INTEGER PRIMARY KEY);",
            "CREATE TABLE a (id INTEGER PRIMARY KEY);   ",
            "CREATE TABLE a (id INTEGER PRIMARY KEY); -- and that is all",
            "CREATE TABLE a (id INTEGER PRIMARY KEY); /* and that is all */",
            "CREATE TABLE a (id INTEGER PRIMARY KEY);;;",
            // A trigger body holds semicolons, which is why counting them is the wrong test.
            "CREATE TRIGGER t AFTER INSERT ON a FOR EACH ROW BEGIN UPDATE a SET id = id; END",
        ] {
            assert_eq!(
                held.trailing_statement(one),
                None,
                "{one:?} is one statement"
            );
        }

        let two = held
            .trailing_statement(
                "CREATE TABLE a (id INTEGER PRIMARY KEY); CREATE TABLE b (id INTEGER PRIMARY KEY)",
            )
            .expect("two statements are two statements");
        assert!(
            two.starts_with("CREATE TABLE b"),
            "the refusal names what comes next, and said {two:?}"
        );

        // A comment between them does not hide the second one.
        let commented = held
            .trailing_statement(
                "CREATE TABLE a (id INTEGER PRIMARY KEY); -- next
CREATE TABLE b (id INTEGER PRIMARY KEY)",
            )
            .expect("a comment does not hide a statement");
        assert!(
            commented.starts_with("CREATE TABLE b"),
            "said {commented:?}"
        );
    }

    /// Text that will not compile is a syntax error, not a script with too many statements in it.
    #[test]
    fn text_that_does_not_compile_is_left_to_the_parser() {
        let held = shell();
        assert_eq!(held.trailing_statement("SELEKT 1"), None);
        assert_eq!(held.trailing_statement(""), None);
        assert_eq!(held.trailing_statement("-- only a comment"), None);
    }
}
