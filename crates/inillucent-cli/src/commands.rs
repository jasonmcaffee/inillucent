//! The dot commands that are about the shell rather than about the database.
//!
//! Invariant: each one does what the reference's does, including when it
//! refuses. A command that printed a different usage line for a missing
//! argument would be a different command, because the usage line is the whole
//! output in the case a script is most likely to hit.
//!
//! What is here is the group that needs nothing from the engine: the working
//! directory, a subprocess, the prompt strings, how output is punctuated, and
//! the two that make the reference's own test scripts run (`.testcase` and
//! `.check`). The ones that ask the database something are in `diagnose`.

use crate::shell::Shell;

/// `.cd DIRECTORY`: change the working directory.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn cd(shell: &mut Shell, arguments: &[&str]) {
    if shell.unsafe_refused(".cd") {
        return;
    }
    let Some(path) = arguments.first() else {
        shell.complain("Usage: .cd DIRECTORY");
        return;
    };
    if std::env::set_current_dir(path).is_err() {
        shell.complain(&format!("Cannot change to directory \"{path}\""));
    }
}

/// `.shell CMD ARGS...` and `.system CMD ARGS...`: run a command.
///
/// Both spellings are the same command and the reference's usage line names
/// `.system` for either, which is why this one does too.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn system(shell: &mut Shell, arguments: &[&str]) {
    if shell.unsafe_refused(".system") {
        return;
    }
    if arguments.is_empty() {
        shell.complain("Usage: .system COMMAND");
        return;
    }
    let line = arguments.join(" ");
    let status = if cfg!(windows) {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(&line)
            .status()
    } else {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&line)
            .status()
    };
    if let Ok(status) = status {
        if !status.success() {
            // The reference reports the child's own exit code and nothing else,
            // because the child has already said whatever it had to say.
            shell.failed = true;
        }
    } else {
        shell.complain("Error: cannot run the system shell");
    }
}

/// `.crlf ?on|off?`: whether output lines end `\r\n`.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn crlf(shell: &mut Shell, arguments: &[&str]) {
    if let Some(word) = arguments.first() {
        shell.crlf = crate::dot::truthy(Some(word));
    }
    let state = if shell.crlf { "ON" } else { "OFF" };
    shell.say(&format!("crlf is {state}"));
}

/// `.prompt MAIN CONTINUE`: the two strings an interactive session prints.
///
/// Recorded and printed by `.show`, which is the only thing that reads them in
/// a script - the prompts themselves are only written when input is a terminal.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn prompt(shell: &mut Shell, arguments: &[&str]) {
    if let Some(main) = arguments.first() {
        shell.prompt_main = (*main).to_string();
    }
    if let Some(more) = arguments.get(1) {
        shell.prompt_continue = (*more).to_string();
    }
}

/// `.explain ?on|off|auto?`: how an `EXPLAIN` listing is laid out.
///
/// `auto` is the default and is what makes an `EXPLAIN` print as a table while
/// everything else keeps the current `.mode`.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn explain(shell: &mut Shell, arguments: &[&str]) {
    shell.explain_mode = match arguments.first().map(|word| word.to_ascii_lowercase()) {
        None => ExplainMode::On,
        Some(word) if word == "auto" => ExplainMode::Auto,
        Some(word) if crate::dot::truthy(Some(&word)) => ExplainMode::On,
        Some(_) => ExplainMode::Off,
    };
}

/// When an `EXPLAIN` listing is laid out as a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainMode {
    /// Only for a statement that is an `EXPLAIN`, which is the default.
    Auto,
    /// For every statement.
    On,
    /// For none, so an `EXPLAIN` prints under the current `.mode`.
    Off,
}

/// `.nonce STRING`: the token that suspends safe mode for one command.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn nonce(shell: &mut Shell, arguments: &[&str]) {
    let Some(value) = arguments.first() else {
        shell.complain("Usage: .nonce NONCE");
        return;
    };
    shell.nonce = Some((*value).to_string());
}

/// `.testcase NAME`: start capturing output for the next `.check`.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn testcase(shell: &mut Shell, arguments: &[&str]) {
    shell.testcase = Some(
        arguments
            .first()
            .map(|name| (*name).to_string())
            .unwrap_or_default(),
    );
    shell.captured.clear();
}

/// `.check TEXT`: compare what has been printed since `.testcase`.
///
/// Silence on a match and a three-line report on a mismatch, which is the
/// reference's own harness protocol - its test scripts are read by both.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn check(shell: &mut Shell, arguments: &[&str]) {
    let line = shell.line;
    let typed = if arguments.is_empty() {
        ".check".to_string()
    } else {
        format!(".check {}", arguments.join(" "))
    };
    let Some(name) = shell.testcase.take() else {
        shell.complain(&format!("line {line}: {typed}"));
        shell.complain(&format!("line {line}:  ^--- no .testcase is active"));
        return;
    };
    shell.tests_run = shell.tests_run.saturating_add(1);
    let wanted = arguments.join(" ");
    // **Compared with the trailing newline the output actually had**, because
    // that is what the reference compares and what its own `[...]` brackets
    // show: a `Got:` that ends inside the bracket and a `]` on the next line is
    // how a missing newline is told from a present one.
    let got = shell.captured.clone();
    if got.trim_end_matches('\n') == wanted {
        shell.captured.clear();
        return;
    }
    shell.tests_failed = shell.tests_failed.saturating_add(1);
    shell.complain(&format!(
        "<stdin>:{line}: .check failed for testcase {name}"
    ));
    shell.complain(&format!("Expected: [{wanted}]"));
    shell.complain(&format!("Got:      [{got}]"));
    shell.captured.clear();
}

/// Reports how many `.testcase`/`.check` pairs ran, at the end of the script.
///
/// Nothing is printed when none ran, which is what makes the line invisible to
/// a script that is not a test script.
///
/// @param shell - the shell
pub fn report_tests(shell: &mut Shell) {
    if shell.tests_run == 0 {
        return;
    }
    let run = shell.tests_run;
    let failed = shell.tests_failed;
    let plural = if failed == 1 { "error" } else { "errors" };
    // The tally goes to standard output, where the reference puts it; the
    // failures themselves go to standard error. A harness that reads the two
    // streams apart sees the count as a result and the failures as diagnostics.
    shell.say(&format!("{run} tests run with {failed} {plural}"));
}

/// `.excel` and `.www`: send the next command's output somewhere to look at.
///
/// The reference writes the rows to a temporary file and hands it to the
/// system's own handler - a spreadsheet for `.excel`, a browser for `.www`.
/// Both are `.once` with a mode and a destination chosen for you.
///
/// @param shell - the shell
/// @param html - whether the file is HTML rather than CSV
pub fn viewer(shell: &mut Shell, html: bool) {
    if shell.unsafe_refused(if html { ".www" } else { ".excel" }) {
        return;
    }
    let suffix = if html { "html" } else { "csv" };
    let path = std::env::temp_dir().join(format!(
        "inillucent-{}.{suffix}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|held| held.as_nanos())
            .unwrap_or(0)
    ));
    shell.layout.mode = if html {
        crate::render::Mode::Html
    } else {
        crate::render::Mode::Csv
    };
    shell.layout.headers = true;
    if let Err(reason) = shell.redirect(Some(&path.to_string_lossy()), true) {
        shell.complain(&format!("Error: {reason}"));
        return;
    }
    shell.viewer = Some(path);
}

/// Hands a `.excel` or `.www` file to the system's own handler.
///
/// Called when the redirected command finishes, which is where the reference
/// opens it too: the file has to be complete before anything is asked to read
/// it.
///
/// @param path - the file that was written
pub fn open_viewer(path: &std::path::Path) {
    let opened = if cfg!(windows) {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(path)
            .status()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).status()
    } else {
        std::process::Command::new("xdg-open").arg(path).status()
    };
    let _ = opened;
}

/// `.scanstats on|off|est`: whether per-statement scan metrics are collected.
///
/// **Accepted and recorded rather than acted on, and the reference's own build
/// is the same.** The metrics come from `sqlite3_stmt_scanstatus`, which is
/// compiled out of the pinned `sqlite3` - `.scanstats on` there prints nothing
/// extra either. What this engine has instead is the `sqlite_stmt` table, whose
/// `nscan`, `nsort` and `naidx` columns are the same three numbers asked for as
/// SQL.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn scanstats(shell: &mut Shell, arguments: &[&str]) {
    let Some(word) = arguments.first() else {
        shell.complain("Usage: .scanstats on|off|est");
        return;
    };
    let folded = word.to_ascii_lowercase();
    if !matches!(folded.as_str(), "on" | "off" | "est") {
        shell.complain("Usage: .scanstats on|off|est");
        return;
    }
    shell.scanstats = folded;
}

/// `.trace ?FILE|off?`: echo each statement before it runs.
///
/// `stdout` and `stderr` name the two streams; anything else is a file; `off`
/// stops. With no argument it stops, which is what the reference does.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn trace(shell: &mut Shell, arguments: &[&str]) {
    let Some(word) = arguments.first() else {
        shell.trace = None;
        return;
    };
    if word.eq_ignore_ascii_case("off") {
        shell.trace = None;
        return;
    }
    shell.trace = Some((*word).to_string());
}

/// `.auth ON|OFF`: print every decision the authorizer is asked to make.
///
/// The reference installs an authorizer that allows everything and writes each
/// callback to standard output, which is what makes the command a window on
/// what a statement actually touches. The lines are its own format:
/// `authorizer:` then the action and four arguments, each either `NULL` or a
/// quoted string.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn auth(shell: &mut Shell, arguments: &[&str]) {
    let Some(word) = arguments.first() else {
        shell.complain("Usage: .auth ON|OFF");
        return;
    };
    let on = crate::dot::truthy(Some(word));
    shell.auth = on;
    shell.set_authorizer(on);
}

/// The authorizer `.auth on` installs: it allows everything and says so.
pub struct Watching {
    /// Where the lines go until the shell prints them.
    pub seen: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl inillucent_engine::Authorizer for Watching {
    /// Records one decision and allows it.
    ///
    /// @param action - what the binder is about to bind
    fn authorize(
        &self,
        action: inillucent_engine::AuthAction<'_>,
    ) -> inillucent_engine::Authorization {
        let line = match action {
            inillucent_engine::AuthAction::Select => "SELECT NULL NULL NULL NULL".to_string(),
            inillucent_engine::AuthAction::Read {
                database,
                table,
                column,
            } => format!(
                "READ {} {} {} NULL",
                quoted(table),
                quoted(column),
                quoted(database)
            ),
            inillucent_engine::AuthAction::Function { name } => {
                format!("FUNCTION NULL {} NULL NULL", quoted(name))
            }
        };
        self.seen.borrow_mut().push(format!("authorizer: {line}"));
        inillucent_engine::Authorization::Allow
    }
}

/// Renders one authorizer argument the way the reference renders it.
///
/// @param bytes - the name
fn quoted(bytes: &[u8]) -> String {
    format!("\"{}\"", String::from_utf8_lossy(bytes))
}

/// `.connection [close] [#]`: open, list or close an auxiliary database.
///
/// Five slots, which is the reference's own limit. With no argument the open
/// ones are listed and the one statements run on is marked `ACTIVE`; with a
/// number the shell switches to that slot, opening an in-memory database there
/// if it was closed; with `close` and a number that slot is closed.
///
/// A number out of range is ignored rather than refused, which is what the
/// reference does with it.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn connection(shell: &mut Shell, arguments: &[&str]) {
    /// How wide the slot number is printed, so `ACTIVE 0:` and `       0:`
    /// put their colons in the same column.
    const MARK: usize = 6;

    match arguments.first().map(|word| word.to_ascii_lowercase()) {
        None => {
            let active = shell.active();
            for (slot, held) in shell.slots().into_iter().enumerate() {
                let Some(path) = held else {
                    continue;
                };
                let mark = if slot == active { "ACTIVE" } else { "" };
                let name = if path == ":memory:" {
                    "(memory)".to_string()
                } else {
                    path
                };
                let line = format!("{mark:<MARK$} {slot}: {name}");
                shell.say(&line);
            }
        }
        Some(word) if word == "close" => {
            if let Some(slot) = arguments.get(1).and_then(|text| text.parse::<usize>().ok()) {
                shell.close_slot(slot);
            }
        }
        Some(word) => {
            if let Ok(slot) = word.parse::<usize>() {
                if let Err(reason) = shell.use_slot(slot) {
                    shell.complain(&format!("Error: {reason}"));
                }
            }
        }
    }
}

/// `.imposter INDEX IMPOSTER` and `.imposter off`: read an index directly.
///
/// An index's entries are the indexed columns followed by the row's identity,
/// which is a `WITHOUT ROWID` table - so declaring one over the index's own
/// b-tree is a way to read what the index actually holds when a query over it
/// is answering wrongly. The declaration is this connection's and is not
/// written to the file.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn imposter(shell: &mut Shell, arguments: &[&str]) {
    match (arguments.first().copied(), arguments.get(1).copied()) {
        (Some(word), None) if word.eq_ignore_ascii_case("off") => {
            if let Err(reason) = shell.connection().imposter(None, b"") {
                shell.complain(&format!("Error: {}", reason.message()));
            }
        }
        (Some(index), Some(name)) => {
            match shell
                .connection()
                .imposter(Some(index.as_bytes()), name.as_bytes())
            {
                Ok(Some(sql)) => shell.say(&sql),
                Ok(None) => {}
                Err(reason) => shell.complain(reason.message()),
            }
        }
        _ => {
            shell.complain("Usage: .imposter INDEX IMPOSTER");
            shell.complain("       .imposter off");
        }
    }
}
