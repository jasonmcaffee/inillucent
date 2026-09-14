//! The command table: one array, read by every front end.
//!
//! Invariant: **a command exists once.** [`COMMANDS`] holds its name, its
//! summary, the parameters it takes and the function that runs it, and the
//! three front ends read that array rather than each carrying a list:
//!
//! - `inillucent <verb>` builds its usage and its argument parsing from it;
//! - `inillucent-mcp` builds `tools/list` and its JSON Schemas from it;
//! - `inillucent help` prints it.
//!
//! `crates/inillucent-compat/tests/command_parity.rs` fails the build if a
//! command loses its description, if a parameter loses one, if a command is
//! hidden from MCP without a stated reason, or if the two surfaces stop naming
//! the same set. That test is the whole point of the arrangement: this
//! repository already argues, in `drivers/README.md`, that a capability list
//! nobody runs decays into a list of claims that were true once, and a command
//! list is the same kind of claim.
//!
//! **Everything here goes through [`crate::shell::Shell`].** The shell is
//! already an adapter over the public facade, and the 416-case differential
//! probe covers that path. A command table that reached past it to the engine
//! would be a second path to the same data, answering slightly differently, and
//! nobody would find out from the tests that exist.

pub mod outcome;
pub mod verbs;

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_driver::Status;
use inillucent_engine::vfs::confine::{self, Root};

use crate::json::Json;
use crate::shell::Shell;

pub use outcome::{Column, Failed, Outcome};

/// What kind of value a parameter takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A string.
    Text,
    /// A whole number.
    Integer,
    /// True or false.
    Boolean,
    /// An array of SQL values, for binding to `?1`, `?2`, ...
    Values,
}

impl Kind {
    /// Returns the JSON Schema type an MCP client is told to send.
    pub fn schema_type(self) -> &'static str {
        match self {
            Kind::Text => "string",
            Kind::Integer => "integer",
            Kind::Boolean => "boolean",
            Kind::Values => "array",
        }
    }

    /// Returns whether a JSON value has this parameter kind.
    ///
    /// @param value - the value a client supplied
    pub fn accepts(self, value: &Json) -> bool {
        match self {
            Kind::Text => matches!(value, Json::Text(_)),
            Kind::Integer => value.integer().is_some(),
            Kind::Boolean => matches!(value, Json::Bool(_)),
            Kind::Values => value.array().is_some_and(|items| {
                items.iter().all(|item| {
                    matches!(
                        item,
                        Json::Null | Json::Bool(_) | Json::Int(_) | Json::Real(_) | Json::Text(_)
                    )
                })
            }),
        }
    }
}

/// One parameter a command takes.
#[derive(Debug, Clone, Copy)]
pub struct Param {
    /// The name, which is the MCP property name and the CLI's `--name`.
    pub name: &'static str,
    /// What kind of value it takes.
    pub kind: Kind,
    /// Whether the command refuses without it.
    pub required: bool,
    /// Whether it can be given as a bare word on the command line.
    ///
    /// At most one positional per command, and it is always the first one: a
    /// command line with two unnamed arguments is one nobody can read back.
    pub positional: bool,
    /// What it is for, in one sentence.
    ///
    /// **This is what the model reads.** A parameter whose description says
    /// "the table" tells an agent nothing it could not guess; one that says
    /// which name form is expected saves a failed call. The parity test refuses
    /// an empty one.
    pub description: &'static str,
}

/// One command.
pub struct Command {
    /// The verb, as `inillucent <name>` and as `inillucent_<name>`.
    pub name: &'static str,
    /// One line, shown in the command list and used as the MCP description.
    pub summary: &'static str,
    /// The longer explanation, shown by `inillucent help <name>` and appended
    /// to the MCP description so a model reads the same thing a person does.
    pub detail: &'static str,
    /// What it takes.
    pub params: &'static [Param],
    /// Why it is not offered over MCP, when it is not.
    pub cli_only: Option<&'static str>,
    /// Whether it can change the database, and so is refused when read-only.
    pub writes: bool,
    /// What it does.
    pub run: fn(&mut Context, &Arguments) -> Result<Outcome, Failed>,
}

impl Command {
    /// Returns this command's parameter of a given name.
    ///
    /// @param name - the parameter name
    pub fn param(&self, name: &str) -> Option<&'static Param> {
        self.params.iter().find(|param| param.name == name)
    }

    /// Returns the parameter that may be written without its name.
    pub fn positional(&self) -> Option<&'static Param> {
        self.params.iter().find(|param| param.positional)
    }

    /// Returns the usage line the CLI prints for this command.
    pub fn usage(&self) -> String {
        let mut line = format!("inillucent {}", self.name);
        for param in self.params {
            let form = match (param.positional, param.required) {
                (true, true) => format!(" <{}>", param.name),
                (true, false) => format!(" [{}]", param.name),
                (false, true) => format!(" --{} <{}>", param.name, param.name),
                (false, false) => format!(" [--{} <{}>]", param.name, param.name),
            };
            line.push_str(&form);
        }
        line
    }

    /// Returns the finite text values a parameter accepts when it has any.
    ///
    /// @param name - the parameter name
    pub fn allowed_values(&self, name: &str) -> Option<&'static [&'static str]> {
        match (self.name, name) {
            (_, "output") => Some(&["text", "json"]),
            ("import", "format") => Some(&["csv", "tabs", "ascii"]),
            ("export", "format") => Some(&[
                "csv", "json", "tabs", "markdown", "insert", "quote", "line", "html",
            ]),
            _ => None,
        }
    }
}

/// The values a command was given.
#[derive(Debug, Clone, Default)]
pub struct Arguments {
    /// Each name and what was passed under it.
    values: Vec<(String, Json)>,
}

impl Arguments {
    /// Builds an argument set from an MCP `arguments` object.
    ///
    /// @param command - the command that declares the accepted arguments
    /// @param object - the object the client sent
    pub fn from_json(command: &Command, object: &Json) -> Result<Arguments, Failed> {
        let Json::Object(pairs) = object else {
            return Err(Failed::misuse("tool arguments must be an object."));
        };
        for (name, value) in pairs {
            let Some(param) = command.param(name) else {
                return Err(Failed::misuse(format!(
                    "'{}' has no '{name}' argument.",
                    command.name
                )));
            };
            if !param.kind.accepts(value) {
                return Err(Failed::misuse(format!(
                    "'{name}' has to be a {}.",
                    param.kind.schema_type()
                )));
            }
            if let Some(allowed) = command.allowed_values(name) {
                let Some(text) = value.text() else {
                    return Err(Failed::misuse(format!("'{name}' has to be text.")));
                };
                if !allowed.contains(&text) {
                    return Err(Failed::misuse(format!(
                        "'{name}' must be one of: {}.",
                        allowed.join(", ")
                    )));
                }
            }
        }
        for param in command.params {
            if param.required && !pairs.iter().any(|(name, _)| name == param.name) {
                return Err(Failed::misuse(format!("'{}' is required.", param.name)));
            }
        }
        Ok(Arguments {
            values: pairs.clone(),
        })
    }

    /// Records one value.
    ///
    /// @param name - the parameter
    /// @param value - what was given
    pub fn set(&mut self, name: &str, value: Json) {
        self.values.retain(|(held, _)| held != name);
        self.values.push((name.to_string(), value));
    }

    /// Returns what was given under a name.
    ///
    /// @param name - the parameter
    pub fn get(&self, name: &str) -> Option<&Json> {
        self.values
            .iter()
            .find(|(held, _)| held == name)
            .map(|(_, value)| value)
    }

    /// Returns a text parameter, if it was given as text.
    ///
    /// @param name - the parameter
    pub fn text(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Json::text)
    }

    /// Returns a text parameter, or the failure for having left it out.
    ///
    /// @param name - the parameter
    pub fn required_text(&self, name: &str) -> Result<&str, Failed> {
        match self.get(name) {
            Some(Json::Text(text)) => Ok(text),
            Some(_) => Err(Failed::misuse(format!("'{name}' has to be a string."))),
            None => Err(Failed::misuse(format!("'{name}' is required."))),
        }
    }

    /// Returns an integer parameter.
    ///
    /// @param name - the parameter
    pub fn integer(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(Json::integer)
    }

    /// Returns a boolean parameter, treating an absent one as false.
    ///
    /// @param name - the parameter
    pub fn flag(&self, name: &str) -> bool {
        self.get(name).and_then(Json::boolean).unwrap_or(false)
    }

    /// Returns the array a `values` parameter carries.
    ///
    /// @param name - the parameter
    pub fn values(&self, name: &str) -> Vec<Json> {
        self.get(name)
            .and_then(Json::array)
            .map(<[Json]>::to_vec)
            .unwrap_or_default()
    }
}

/// Where a command runs: the database, and the limits placed on it.
pub struct Context {
    /// The shell every command drives.
    shell: Shell,
    /// The file it is open on.
    path: String,
    /// Whether a statement that changes anything is refused.
    readonly: bool,
    /// The directory outside which no path may be named.
    ///
    /// The same [`Root`] the whole process is confined to, so a refusal a
    /// person reads and a refusal the file system enforces are one decision
    /// rather than two that can disagree.
    root: Option<Arc<Root>>,
    /// How many rows a command hands back when it was not told.
    pub limit: usize,
    /// The most rows one call may hand back, when this surface has a ceiling.
    max_rows: Option<usize>,
    /// What one command may spend inside the engine.
    ///
    /// Different from `max_rows`, and both are needed. `max_rows` bounds what a
    /// call *hands back*; this bounds what the engine does on the way there, so
    /// a `SELECT` whose `WHERE` rejects everything after scanning a hundred
    /// million rows still stops. A row ceiling alone would let that run to the
    /// end and then report zero rows.
    limits: inillucent_engine::base::budget::Limits,
    /// Whether arming a budget clears the cancellation flag first.
    ///
    /// True everywhere but the MCP server, which reads its input on a second
    /// thread and therefore owns the ordering itself; see
    /// `budget::arm_as_it_stands` (task-1932, H11).
    preserve_cancel: std::cell::Cell<bool>,
    /// The flag that stops whatever this session is running.
    ///
    /// **One per session, not one per call (task-1932, H11).** `run` used to
    /// arm the budget with a fresh `AtomicBool` it dropped on the way out, so
    /// the flag the executor polled every batch was one nothing else in the
    /// process had a handle to: `Connection::cancel` in the driver was correct
    /// and unreachable, and an MCP `notifications/cancelled` had nothing to
    /// set. Handing out a clone of this is what makes a cancel arriving from
    /// another thread land on the statement that is running.
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// What to print where a value is null.
    pub null: String,
}

impl Context {
    /// Opens a context on a database.
    ///
    /// @param path - the file, or an in-memory name
    /// @param readonly - whether writes are refused
    /// @param root - the directory paths are confined to, if any
    pub fn open(path: &str, readonly: bool, root: Option<PathBuf>) -> Result<Context, Failed> {
        // **The confinement is installed before the first file is opened.**
        // The database this surface starts on is a path like any other, and
        // installing the root afterwards would exempt exactly the one path an
        // operator is most likely to have got wrong. It also puts the root
        // where the VFS can see it, which is what confines every file the
        // engine opens later without this module having to name them.
        let root = match root {
            None => None,
            Some(directory) => {
                confine::confine_process(&directory).map_err(|error| {
                    Failed::said(Status::InvalidState, error.detail().to_string())
                })?;
                confine::process_root()
            }
        };
        let opened = match &root {
            Some(root) => root
                .admit(path)
                .map_err(|refused| Failed::said(Status::InvalidState, refused.message()))?
                .to_string_lossy()
                .into_owned(),
            None => path.to_string(),
        };
        let shell = Shell::open(&opened).map_err(|message| {
            Failed::said(
                Status::Io,
                format!("could not open \"{opened}\": {message}"),
            )
        })?;
        Ok(Context {
            shell,
            path: opened,
            readonly,
            root,
            limit: 200,
            max_rows: None,
            limits: inillucent_engine::base::budget::Limits::unbounded(),
            preserve_cancel: std::cell::Cell::new(false),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            null: String::new(),
        })
    }

    /// Points this context at a different file, reopening only if it moved.
    ///
    /// **One open database at a time, and that is a decision rather than a
    /// simplification.** One file is one buffer pool, and a server holding four
    /// of them holds four pools whose sizes nobody asked about. A caller that
    /// alternates pays a reopen; a caller that does not pays nothing.
    ///
    /// @param path - the file to use
    pub fn use_database(&mut self, path: &str) -> Result<(), Failed> {
        if path == self.path {
            return Ok(());
        }
        let confined = self.confine(path)?;
        let named = confined.to_string_lossy().into_owned();
        self.shell.reopen(&named).map_err(|message| {
            Failed::said(Status::Io, format!("could not open \"{named}\": {message}"))
        })?;
        self.path = named;
        Ok(())
    }

    /// Returns a handle to this session's cancellation flag.
    ///
    /// Setting it stops the statement that is running, at the next batch. It is
    /// cleared when the next command is armed, so a cancel that arrives between
    /// two calls belongs to the one that has finished and is discarded rather
    /// than applied to the one that has not started.
    pub fn cancel_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.cancel)
    }

    /// Says that this surface clears the cancellation flag itself.
    ///
    /// Only `inillucent-mcp` does, because it is the only one that reads its
    /// input on a second thread. See `budget::arm_as_it_stands`.
    pub fn preserve_cancellation(&self) {
        self.preserve_cancel.set(true);
    }

    /// Returns the shell commands drive.
    pub fn shell(&mut self) -> &mut Shell {
        &mut self.shell
    }

    /// Returns the file this context is open on.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns whether writes are refused.
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    /// Refuses a row count past this surface's ceiling, when it has one.
    ///
    /// **The command line has no ceiling and the MCP server does**, which is
    /// the whole distinction: a person running `inillucent query` against their
    /// own database and asking for every row is asking for what they want, and
    /// an agent doing the same thing to a served database is the case `--root`
    /// and `--readonly` already exist for. Zero means every row and is refused
    /// where a ceiling is set, because "every row" is precisely the request the
    /// ceiling is about.
    ///
    /// @param asked - the row count the caller wants
    pub fn cap_rows(&self, asked: usize) -> Result<usize, Failed> {
        let Some(most) = self.max_rows else {
            return Ok(asked);
        };
        match asked {
            0 => Err(Failed::said(
                Status::InvalidState,
                format!(
                    "limit=0 asks for every row, and this server hands back at most {most}. Ask \
                     for a count, or narrow the query."
                ),
            )),
            asked if asked > most => Err(Failed::said(
                Status::InvalidState,
                format!("limit={asked} is past the {most} rows this server hands back."),
            )),
            asked => Ok(asked),
        }
    }

    /// Sets the ceiling on how many rows one call hands back.
    ///
    /// @param most - the ceiling, or `None` for the command line's absence of one
    pub fn set_max_rows(&mut self, most: Option<usize>) {
        self.max_rows = most;
    }

    /// Sets what one command may spend inside the engine.
    ///
    /// @param limits - the budget, or `Limits::unbounded` for a command line
    pub fn set_limits(&mut self, limits: inillucent_engine::base::budget::Limits) {
        self.limits = limits;
    }

    /// Returns what one command on this surface may spend inside the engine.
    ///
    /// A verb that runs work outside the executor - a migration reads a remote
    /// server and writes rows through a second connection - asks so that it can
    /// put itself under the same ceiling rather than beside it.
    pub fn limits(&self) -> inillucent_engine::base::budget::Limits {
        self.limits.clone()
    }

    /// Returns whether this surface was confined to a directory.
    ///
    /// **Confinement is about reach, not only about paths.** `--root` exists so
    /// that an MCP server can be handed to an agent without handing it the file
    /// system, and a verb that dialled a host and a port would be a hole
    /// straight through it. A command that can reach something other than a
    /// file asks this and refuses.
    pub fn confined(&self) -> bool {
        self.root.is_some()
    }

    /// Returns a surface over a shell a test already opened.
    ///
    /// Here rather than in each test module because `Context`'s fields are
    /// private to this module, and a test that reached into them would be a
    /// second definition of what a surface is.
    ///
    /// @param shell - the shell to drive
    /// @param root - the directory to confine to, when there is one
    #[cfg(test)]
    pub fn for_test(shell: Shell, root: Option<PathBuf>) -> Context {
        Context {
            shell,
            path: ":memory:".to_string(),
            readonly: false,
            // A test builds its own root rather than installing a process-wide
            // one: the process root is set once for the life of the process,
            // and a test that installed it would decide the confinement of
            // every other test in the binary.
            root: root.map(|directory| {
                Arc::new(Root::resolved(confine::resolve_through_links(&directory)))
            }),
            limit: 200,
            max_rows: None,
            limits: inillucent_engine::base::budget::Limits::unbounded(),
            preserve_cancel: std::cell::Cell::new(false),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            null: String::new(),
        }
    }

    /// Refuses a path outside the root, when a root was set.
    ///
    /// **The decision is not made here.** It is made by
    /// [`inillucent_vfs::confine`], which resolves the path through the file
    /// system rather than reading its text, and which the VFS consults again
    /// at the moment the file is opened. This method exists so that a person
    /// reading a refusal is told the path they typed and the directory they
    /// confined to, neither of which survives as far as the VFS.
    ///
    /// The check this replaced compared normalised path text against the root.
    /// A junction below the root passed it and opened a database outside the
    /// root.
    ///
    /// @param path - the path a caller named
    pub fn confine(&self, path: &str) -> Result<PathBuf, Failed> {
        let Some(root) = &self.root else {
            return Ok(PathBuf::from(path));
        };
        root.admit(path)
            .map_err(|refused| Failed::said(Status::InvalidState, refused.message()))
    }

    /// Refuses a statement that changes something, when read-only.
    ///
    /// **The binder decides, not a scan of the text.** `EXPLAIN QUERY PLAN`
    /// over the statement fails with "not a read-only statement" for anything
    /// that writes, which is the engine's own classification and cannot be
    /// talked past with whitespace, a comment or an unusual capitalisation.
    /// It is the same mechanism `inillucent-driver` uses, for the same reason.
    ///
    /// @param sql - the statement
    pub fn refuse_if_it_writes(&self, sql: &str) -> Result<(), Failed> {
        if !self.readonly {
            return Ok(());
        }
        match self.shell.connection().explain(sql) {
            Ok(_) => Ok(()),
            Err(error) => {
                let classified = Failed::from_engine(&error);
                match classified.message.contains("not a read-only statement") {
                    true => Err(Failed::said(
                        Status::ReadOnly,
                        "this connection is read only, and that statement changes something.",
                    )),
                    // A statement that failed to plan for any other reason is
                    // that failure, and reporting it as a read-only refusal
                    // would tell a caller to reopen the file over a typo.
                    false => Err(classified),
                }
            }
        }
    }

    /// Runs shell input, collecting everything it printed.
    ///
    /// @param input - the lines, dot commands included
    pub fn collect_output(&mut self, input: &str) -> String {
        self.shell.sink = Some(String::new());
        let lines: Vec<String> = input.lines().map(str::to_string).collect();
        crate::shell::drive(&mut self.shell, lines.into_iter());
        self.shell.sink.take().unwrap_or_default()
    }
}

/// Returns the command of a given name.
///
/// @param name - the verb, with or without the `inillucent_` prefix MCP uses
pub fn find(name: &str) -> Option<&'static Command> {
    let bare = name.strip_prefix("inillucent_").unwrap_or(name);
    // A dash reads better on a command line and an underscore is required in an
    // MCP tool name, so both spellings find the same command rather than one of
    // them being a mistake a caller has to learn about.
    let wanted = bare.replace('-', "_");
    COMMANDS
        .iter()
        .find(|command| command.name.replace('-', "_") == wanted)
}

/// Runs a command, applying the checks every front end shares.
///
/// The order is the whole of it: the database is selected first because a
/// confinement refusal must happen before anything is opened, the read-only
/// check is second because a refusal is cheaper than a run, and only then does
/// the command see its arguments.
///
/// @param command - what to run
/// @param context - where to run it
/// @param arguments - what it was given
pub fn run(
    command: &'static Command,
    context: &mut Context,
    arguments: &Arguments,
) -> Result<Outcome, Failed> {
    if let Some(path) = arguments.text("db") {
        context.use_database(path)?;
    }
    if command.writes && context.readonly() {
        return Err(Failed::said(
            Status::ReadOnly,
            format!(
                "'{}' changes the database, and this is read only.",
                command.name
            ),
        ));
    }
    for param in command.params {
        if param.required && arguments.get(param.name).is_none() {
            return Err(Failed::misuse(format!(
                "'{}' needs '{}'. Usage: {}",
                command.name,
                param.name,
                command.usage()
            )));
        }
    }
    let started = std::time::Instant::now();
    // **Armed here, which is the one place every command on every surface goes
    // through.** Arming it inside each verb would be arming it in nineteen
    // places and forgetting it in the twentieth; arming it in the engine would
    // put a server's policy inside a library an application also links.
    let armed = match context.preserve_cancel.get() {
        true => inillucent_driver::arm_as_it_stands(context.limits.clone(), context.cancel_flag()),
        false => inillucent_driver::arm(context.limits.clone(), context.cancel_flag()),
    };
    let outcome = (command.run)(context, arguments);
    drop(armed);
    let mut produced = outcome?;
    if produced.elapsed_ms == 0.0 {
        produced.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    }
    Ok(produced)
}

/// The parameter every command takes, so a caller can name the file per call.
const DB: Param = Param {
    name: "db",
    kind: Kind::Text,
    required: false,
    positional: false,
    description: "The database file to run against. Defaults to the one this process was started \
                  on, or :memory: for a scratch database that is discarded when the process ends.",
};

/// The parameter the row-producing commands take.
const LIMIT: Param = Param {
    name: "limit",
    kind: Kind::Integer,
    required: false,
    positional: false,
    description:
        "How many rows to hand back. The count in 'total' is still exact, and 'more' says \
                  whether anything was cut off. Defaults to 200. Over MCP there is a ceiling \
                  of 10000 rows and 0 (every row) is refused; on the command line there is \
                  neither. A negative number is refused on both.",
};

/// The parameter that switches between the two renderings.
///
/// **Called `output` and not `format`, because `export` already has a
/// `format`** - and that one means CSV against JSON against Markdown, which is
/// a different question from whether the *result object* is drawn as a table or
/// written as JSON. The collision was real rather than theoretical: with both
/// called `format`, `inillucent export people --format json` was read as "draw
/// the result object as JSON" and quietly wrote CSV.
const FORMAT: Param = Param {
    name: "output",
    kind: Kind::Text,
    required: false,
    positional: false,
    description: "'text' for an aligned table a person reads, or 'json' for the whole result object, with typed values, exact counts and the failure class. Defaults to text.",
};

mod registry;

pub use registry::COMMANDS;

#[cfg(test)]
mod tests {
    use super::*;

    /// Both spellings of a name find the same command.
    #[test]
    fn a_name_is_found_either_way() {
        assert!(find("query").is_some());
        assert!(find("inillucent_query").is_some());
        assert_eq!(
            find("integrity_check").map(|command| command.name),
            find("integrity-check").map(|command| command.name)
        );
        assert!(find("nonsense").is_none());
    }

    /// Every command has a summary, a detail, and described parameters.
    #[test]
    fn every_command_is_described() {
        for command in COMMANDS {
            assert!(
                !command.summary.is_empty(),
                "{} has no summary",
                command.name
            );
            assert!(!command.detail.is_empty(), "{} has no detail", command.name);
            for param in command.params {
                assert!(
                    !param.description.is_empty(),
                    "{}.{} has no description",
                    command.name,
                    param.name
                );
            }
        }
    }

    /// A command has at most one positional parameter, and it is the first.
    #[test]
    fn at_most_one_positional_and_it_comes_first() {
        for command in COMMANDS {
            let positions: Vec<usize> = command
                .params
                .iter()
                .enumerate()
                .filter(|(_, param)| param.positional)
                .map(|(nth, _)| nth)
                .collect();
            assert!(positions.len() <= 1, "{} has two positionals", command.name);
            if let Some(first) = positions.first() {
                assert_eq!(*first, 0, "{}'s positional is not first", command.name);
            }
        }
    }

    /// No command's name is repeated.
    #[test]
    fn names_are_unique() {
        let mut seen: Vec<&str> = COMMANDS.iter().map(|command| command.name).collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total);
    }

    /// A confinement refuses a path that climbs out of the root.
    ///
    /// The root here is a directory that exists, because the service resolves
    /// a candidate through the file system and a root that is not there would
    /// make every case below pass for the wrong reason.
    #[test]
    fn confinement_refuses_a_path_that_climbs_out() {
        let root = std::env::temp_dir().join("inillucent-cli-confine");
        std::fs::create_dir_all(&root).unwrap();
        let context = Context {
            shell: Shell::open(":memory:").unwrap(),
            path: ":memory:".to_string(),
            readonly: false,
            root: Some(Arc::new(Root::at(&root).unwrap())),
            limit: 200,
            max_rows: None,
            limits: inillucent_engine::base::budget::Limits::unbounded(),
            preserve_cancel: std::cell::Cell::new(false),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            null: String::new(),
        };
        assert!(context.confine("inner/app.rdb").is_ok());
        assert!(context.confine("../outside.rdb").is_err());
        assert!(context.confine("C:/elsewhere/app.rdb").is_err());
        assert!(context.confine(":memory:").is_ok());
    }

    /// A path that reaches outside the root through a link is refused, and the
    /// refusal names where it landed.
    ///
    /// The unit-level half of `crates/inillucent-compat/tests/confinement.rs`:
    /// that suite proves the shipped binaries refuse it, and this one proves
    /// the message a person reads says which of the two things went wrong.
    #[test]
    fn a_refusal_through_a_link_names_the_target() {
        let base = std::env::temp_dir().join("inillucent-cli-confine-link");
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join("escape");
        if !link.exists() {
            #[cfg(windows)]
            let made = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .output()
                .map(|produced| produced.status.success())
                .unwrap_or(false);
            #[cfg(unix)]
            let made = std::os::unix::fs::symlink(&outside, &link).is_ok();
            if !made {
                return;
            }
        }
        let context = Context {
            shell: Shell::open(":memory:").unwrap(),
            path: ":memory:".to_string(),
            readonly: false,
            root: Some(Arc::new(Root::at(&root).unwrap())),
            limit: 200,
            max_rows: None,
            limits: inillucent_engine::base::budget::Limits::unbounded(),
            preserve_cancel: std::cell::Cell::new(false),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            null: String::new(),
        };
        let failure = context
            .confine("escape/app.rdb")
            .expect_err("a link out of the root is refused");
        assert!(
            failure.message.contains("resolves to"),
            "the refusal did not say where the path landed: {}",
            failure.message
        );
    }
}
