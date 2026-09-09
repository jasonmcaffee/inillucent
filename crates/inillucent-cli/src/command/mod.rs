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

use std::path::{Path, PathBuf};

use inillucent_driver::Status;

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
    /// @param object - the object the client sent, or any other value
    pub fn from_json(object: &Json) -> Arguments {
        match object {
            Json::Object(pairs) => Arguments {
                values: pairs.clone(),
            },
            _ => Arguments::default(),
        }
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
    root: Option<PathBuf>,
    /// How many rows a command hands back when it was not told.
    pub limit: usize,
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
        let shell = Shell::open(path).map_err(|message| {
            Failed::said(Status::Io, format!("could not open \"{path}\": {message}"))
        })?;
        Ok(Context {
            shell,
            path: path.to_string(),
            readonly,
            root,
            limit: 200,
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
            root,
            limit: 200,
            null: String::new(),
        }
    }

    /// Refuses a path outside the root, when a root was set.
    ///
    /// The check is on the *lexical* path after normalising `..`, and it is
    /// applied before the file is opened - a check that resolved the path
    /// through the file system would have to create it first, and a confinement
    /// that has already touched the disk is not a confinement.
    ///
    /// @param path - the path a caller named
    pub fn confine(&self, path: &str) -> Result<PathBuf, Failed> {
        let Some(root) = &self.root else {
            return Ok(PathBuf::from(path));
        };
        if path == ":memory:" {
            return Ok(PathBuf::from(path));
        }
        let joined = match Path::new(path).is_absolute() {
            true => PathBuf::from(path),
            false => root.join(path),
        };
        let normalised = normalise(&joined);
        if normalised.starts_with(root) {
            Ok(normalised)
        } else {
            Err(Failed::said(
                Status::InvalidState,
                format!(
                    "\"{path}\" is outside {}, which this server is confined to.",
                    root.display()
                ),
            ))
        }
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

/// Resolves `.` and `..` without touching the file system.
///
/// @param path - the path to normalise
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
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
    let mut produced = (command.run)(context, arguments)?;
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
                  whether anything was cut off. Defaults to 200.",
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
    #[test]
    fn confinement_refuses_a_path_that_climbs_out() {
        let context = Context {
            shell: Shell::open(":memory:").unwrap(),
            path: ":memory:".to_string(),
            readonly: false,
            root: Some(PathBuf::from("C:/root")),
            limit: 200,
            null: String::new(),
        };
        assert!(context.confine("inner/app.rdb").is_ok());
        assert!(context.confine("../outside.rdb").is_err());
        assert!(context.confine("C:/elsewhere/app.rdb").is_err());
        assert!(context.confine(":memory:").is_ok());
    }
}
