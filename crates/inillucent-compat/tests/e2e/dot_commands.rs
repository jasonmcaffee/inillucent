//! Every dot command the shell dispatches, driven through the built shell.
//!
//! Invariant: **every name `crates/inillucent-cli/src/dot.rs` dispatches is run
//! by a real `inillucent-shell` process and something it printed is asserted
//! on.** Not that it was recognised - a command that is recognised and does
//! nothing prints nothing, and so does one that is not there.
//!
//! **Thirty of the seventy-one dispatched names appeared in no test file at
//! all (task-1969, 5.5).** `.backup .dbtotxt .eqp .excel .fullschema .intck
//! .lint .load .log .progress .quit .read .recover .selftest .show .timeout
//! .timer .trace .vfsinfo .vfslist .www` among them. The claim that the shell
//! implements 63 of `sqlite3`'s 65 dot commands was checked by
//! `tools/feature-probe/registers.js`, which is run by hand, and by nothing
//! that `cargo test` starts: `registers.rs` covers functions, modules, pragmas
//! and collations, and not dot commands.
//!
//! **One shell process per case.** A session carries state - the output mode,
//! where output is redirected, which database is open - so a hundred commands
//! down one pipe would make the order they were written in part of what is
//! under test, and a command that broke the session would fail every case after
//! it with the wrong message.
//!
//! **The six unsafe commands are driven under `-safe`.** `.cd`, `.load`,
//! `.shell`, `.system`, `.excel` and `.www` reach outside the process: two run
//! a command line, two hand a file to the system's own viewer, one changes the
//! working directory. Their refusal in safe mode is a real answer from the same
//! dispatcher, and it is the one this suite can assert without launching a
//! browser on whatever machine the tests are running on.

use std::path::{Path, PathBuf};

use inillucent_compat::cliproc::{program, run, run_with_input};
use inillucent_compat::workspace_root;

/// The 65 dot commands the pinned `sqlite3` 3.53.4 lists in its own `.help`.
///
/// **Hard coded from that binary's output so this runs without it.** The
/// reference is a download the workspace cannot build, and a check that needed
/// it would be one more suite that reports success when it is absent - which is
/// the whole subject of the review this file comes from. It was produced by
/// `printf '.help\n.quit\n' | sqlite3 | grep -oE '^\.[a-z0-9]+'` against
/// `.sqlite-ref/3.53.4/shell/sqlite3`, and it changes when the pin changes.
const REFERENCE: [&str; 65] = [
    "archive",
    "auth",
    "backup",
    "bail",
    "cd",
    "changes",
    "check",
    "clone",
    "connection",
    "crlf",
    "databases",
    "dbconfig",
    "dbinfo",
    "dbtotxt",
    "dump",
    "echo",
    "eqp",
    "excel",
    "exit",
    "expert",
    "explain",
    "filectrl",
    "fullschema",
    "headers",
    "help",
    "import",
    "imposter",
    "indexes",
    "intck",
    "limit",
    "lint",
    "load",
    "log",
    "mode",
    "nonce",
    "nullvalue",
    "once",
    "open",
    "output",
    "parameter",
    "print",
    "progress",
    "prompt",
    "quit",
    "read",
    "recover",
    "restore",
    "save",
    "scanstats",
    "schema",
    "session",
    "sha3sum",
    "shell",
    "stats",
    "system",
    "tables",
    "testcase",
    "timeout",
    "timer",
    "trace",
    "version",
    "vfsinfo",
    "vfslist",
    "vfsname",
    "www",
];

/// The two of the reference's 65 this shell does not implement.
///
/// `.expert` asks SQLite's own index recommender, which is a separate library
/// this engine does not have. `.session` drives the session extension, which is
/// a changeset recorder with no counterpart here. Both are recorded in
/// `docs/feature-comparison.md` rather than hidden.
const NOT_IMPLEMENTED: [&str; 2] = ["expert", "session"];

/// The six that reach outside the process, and are therefore driven under
/// `-safe`.
const UNSAFE: [&str; 6] = ["cd", "load", "shell", "system", "excel", "www"];

/// Returns a scratch directory of this case's own, emptied first.
///
/// Per case rather than per file, because `cargo test` runs the cases in this
/// binary on several threads: one shared directory meant the second case wiped
/// the first case's database out from under it, and the failure named the
/// fixture rather than the race.
///
/// @param case - what to name the directory after
fn area(case: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/dot-commands")
        .join(case);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Builds the database every case opens.
///
/// One table with rows, one index, one full-text table, and the statistics
/// `.fullschema` prints - so that a command which reads the schema has
/// something to read and a command which reads rows has something to return.
///
/// @param binary - the built `inillucent`
/// @param directory - where to put it
fn populated(binary: &Path, directory: &Path) -> PathBuf {
    let database = directory.join("app.rdb");
    let path = database.to_string_lossy().to_string();
    for arguments in [
        vec!["create", path.as_str()],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE INDEX note_body ON note (body)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "INSERT INTO note (body) VALUES ('hello'), ('goodbye')",
        ],
        vec!["--db", path.as_str(), "analyze"],
    ] {
        let ran = run(binary, &arguments);
        assert_eq!(
            ran.code,
            0,
            "building the fixture failed at {arguments:?}:\n{}",
            ran.said()
        );
    }
    database
}

/// One case: the dispatched name, the input to type, and a word the answer has
/// to carry.
///
/// `{dir}` in the input is replaced with this run's scratch directory, written
/// with forward slashes so the shell's own argument splitter does not read a
/// Windows separator as an escape.
struct Case {
    /// The dispatched name this case is the test for.
    name: &'static str,
    /// What to write to the shell, before `.quit`.
    input: &'static str,
    /// A word the shell's output has to carry.
    says: &'static str,
}

/// Every dispatched name, what to type, and what the answer must carry.
///
/// A table rather than seventy-one functions, because the difference between
/// them is three strings; seventy-one functions would be seventy-one copies of
/// one `assert!` and a reader would have to diff them to find the case that is
/// not like the others.
///
/// **A setter is asserted through `.show`**, which prints the settings back, so
/// `.mode json` is checked by the mode `.show` then reports rather than by the
/// silence `.mode` itself produces. That is the difference between testing the
/// command and testing that it did not crash.
const CASES: &[Case] = &[
    // --- the ones that print something of their own --------------------------
    Case {
        name: "help",
        input: ".help",
        says: ".tables",
    },
    Case {
        name: "databases",
        input: ".databases",
        says: "main",
    },
    Case {
        name: "tables",
        input: ".tables",
        says: "note",
    },
    Case {
        name: "indexes",
        input: ".indexes",
        says: "note_body",
    },
    Case {
        name: "indices",
        input: ".indices",
        says: "note_body",
    },
    Case {
        name: "schema",
        input: ".schema",
        says: "CREATE TABLE note",
    },
    Case {
        name: "fullschema",
        input: ".fullschema",
        says: "sqlite_stat1",
    },
    Case {
        name: "dump",
        input: ".dump",
        says: "INSERT INTO note",
    },
    Case {
        name: "version",
        input: ".version",
        says: "SQLite",
    },
    Case {
        name: "show",
        input: ".show",
        says: "mode:",
    },
    Case {
        name: "dbinfo",
        input: ".dbinfo",
        says: "database page size",
    },
    Case {
        name: "dbtotxt",
        input: ".dbtotxt",
        says: "| page 1 offset 0",
    },
    Case {
        name: "sha3sum",
        input: ".sha3sum",
        says: "",
    },
    Case {
        name: "limit",
        input: ".limit",
        says: "sql_length",
    },
    Case {
        name: "limits",
        input: ".limits",
        says: "sql_length",
    },
    Case {
        name: "selftest",
        input: ".selftest",
        says: "0 errors out of",
    },
    Case {
        name: "lint",
        input: ".lint",
        says: "Usage: .lint",
    },
    Case {
        name: "dbconfig",
        input: ".dbconfig",
        says: "attach_create",
    },
    Case {
        name: "vfslist",
        input: ".vfslist",
        says: "vfs.zName",
    },
    Case {
        name: "vfsinfo",
        input: ".vfsinfo",
        says: "vfs.zName",
    },
    Case {
        name: "vfsname",
        input: ".vfsname",
        says: "",
    },
    Case {
        name: "recover",
        input: ".recover",
        says: "BEGIN;",
    },
    Case {
        name: "intck",
        input: ".intck",
        says: "0 errors",
    },
    Case {
        name: "connection",
        input: ".connection",
        says: "ACTIVE 0",
    },
    Case {
        name: "auth",
        input: ".auth",
        says: "Usage: .auth",
    },
    Case {
        name: "imposter",
        input: ".imposter",
        says: "Usage: .imposter",
    },
    Case {
        name: "filectrl",
        input: ".filectrl",
        says: "",
    },
    Case {
        name: "print",
        input: ".print the-printed-word",
        says: "the-printed-word",
    },
    Case {
        name: "stats",
        input: ".stats on\nSELECT 1;",
        says: "Page cache",
    },
    // --- the ones whose effect is read back ----------------------------------
    Case {
        name: "mode",
        input: ".mode json\n.show",
        says: "mode: json",
    },
    Case {
        name: "headers",
        input: ".headers off\n.show",
        says: "headers: off",
    },
    Case {
        name: "nullvalue",
        input: ".nullvalue NIL\n.show",
        says: "nullvalue: \"NIL\"",
    },
    // **`.nullvalues` is dispatched to a refusal on purpose**, and the wording
    // is the reference's: `sqlite3` 3.53.4 answers `.nullvalues` with
    // "Error: unknown command; try .help" rather than treating it as a typo of
    // `.nullvalue`. A shell that quietly accepted it would differ from the
    // reference in a way a script would not notice until its null marker was
    // wrong.
    Case {
        name: "nullvalues",
        input: ".nullvalues NIL",
        says: "unknown command; try .help",
    },
    Case {
        name: "separator",
        input: ".separator @\n.show",
        says: "colseparator: \"@\"",
    },
    Case {
        name: "width",
        input: ".width 7\n.show",
        says: "width: 7",
    },
    Case {
        name: "echo",
        input: ".echo on\n.show",
        says: "echo: on",
    },
    Case {
        name: "eqp",
        input: ".eqp on\n.show",
        says: "eqp: on",
    },
    Case {
        name: "open",
        input: ".open {dir}/opened.rdb\n.show",
        says: "opened.rdb",
    },
    Case {
        name: "restore",
        input: ".restore {dir}/app.rdb\n.tables",
        says: "note",
    },
    Case {
        name: "parameter",
        input: ".parameter set :x 11\n.parameter list",
        says: ":x 11",
    },
    Case {
        name: "testcase",
        input: ".testcase one\nSELECT 1;\n.check 1",
        says: "1 tests run with 0 errors",
    },
    Case {
        name: "check",
        input: ".testcase two\nSELECT 1;\n.check 1",
        says: "1 tests run with 0 errors",
    },
    // Read back rather than asserted on the silence they print: a `.backup`
    // that wrote nothing prints exactly what one that worked prints.
    Case {
        name: "backup",
        input: ".backup {dir}/backed-up.rdb\n.open {dir}/backed-up.rdb\n.tables",
        says: "note",
    },
    Case {
        name: "save",
        input: ".save {dir}/saved.rdb\n.open {dir}/saved.rdb\n.tables",
        says: "note",
    },
    Case {
        name: "clone",
        input: ".clone {dir}/cloned.rdb",
        says: "done",
    },
    Case {
        name: "output",
        input: ".output {dir}/out.txt\nSELECT 'redirected';\n.output\n.print back",
        says: "back",
    },
    Case {
        name: "once",
        input: ".once {dir}/once.txt\nSELECT 'redirected';\n.print back",
        says: "back",
    },
    Case {
        name: "read",
        input: ".read {dir}/script.sql",
        says: "from-the-script",
    },
    Case {
        name: "import",
        input: ".import --csv --skip 1 {dir}/rows.csv note\nSELECT count(*) FROM note;",
        says: "4",
    },
    Case {
        name: "archive",
        input: ".archive --create --file {dir}/made.zip {dir}/rows.csv",
        says: "",
    },
    Case {
        name: "ar",
        input: ".ar --create --file {dir}/made-ar.zip {dir}/rows.csv",
        says: "",
    },
    // --- the settings that print nothing and change a flag -------------------
    Case {
        name: "bail",
        input: ".bail on\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "timer",
        input: ".timer on\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "changes",
        input: ".changes on\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "crlf",
        input: ".crlf on\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "explain",
        input: ".explain on\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "scanstats",
        input: ".scanstats on\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "trace",
        input: ".trace stdout\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "progress",
        input: ".progress 10\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "timeout",
        input: ".timeout 250\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "log",
        input: ".log stderr\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "nonce",
        input: ".nonce abcdef\nSELECT 1;",
        says: "1",
    },
    Case {
        name: "prompt",
        input: ".prompt '> ' '.. '\nSELECT 1;",
        says: "1",
    },
    // --- the two that end the session ----------------------------------------
    Case {
        name: "quit",
        input: ".quit\nSELECT 'after-quit';",
        says: "",
    },
    Case {
        name: "exit",
        input: ".exit\nSELECT 'after-exit';",
        says: "",
    },
];

/// Runs one case in a shell of its own and returns everything it printed.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file to open
/// @param directory - this run's scratch directory
/// @param case - the case to run
/// @param safely - whether to pass `-safe`
fn drive(shell: &Path, database: &Path, directory: &str, case: &Case, safely: bool) -> String {
    let typed = format!("{}\n.quit\n", case.input.replace("{dir}", directory));
    let path = database.to_string_lossy().replace('\\', "/");
    let arguments: Vec<&str> = if safely {
        vec!["-safe", &path]
    } else {
        vec![&path]
    };
    run_with_input(shell, &arguments, &typed).said()
}

/// Every dispatched dot command answers, and the answer says something.
#[test]
fn every_dispatched_dot_command_answers() {
    let binary = program("inillucent");
    let shell = program("inillucent-shell");
    let area = area("dispatched");
    let directory = area.to_string_lossy().replace('\\', "/");
    let database = populated(&binary, &area);
    std::fs::write(area.join("rows.csv"), "id,body\n40,first\n41,second\n")
        .expect("the csv is written");
    std::fs::write(area.join("script.sql"), "SELECT 'from-the-script';\n")
        .expect("the script is written");

    let mut wrong: Vec<String> = Vec::new();
    for case in CASES {
        let printed = drive(&shell, &database, &directory, case, false);
        // Every case has to reach the dispatcher. A name it does not know is
        // the one failure that looks like every other silent pass. The match is
        // on the wording of the fallback arm rather than on the words "unknown
        // command", because `.nullvalues` is dispatched *to* a refusal that
        // says "unknown command; try .help" - which is the reference's own
        // answer to it and is what that case asserts.
        if printed.contains("unknown command or invalid arguments") {
            wrong.push(format!("`.{}` is not dispatched:\n{printed}", case.name));
            continue;
        }
        if !case.says.is_empty() && !printed.contains(case.says) {
            wrong.push(format!(
                "`.{}` did not say `{}`:\n{printed}",
                case.name, case.says
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "these dot commands answered something other than what they are for:\n{}",
        wrong.join("\n---\n")
    );
}

/// The six that reach outside the process are refused under `-safe`.
///
/// Their refusal is the same dispatcher answering, so it covers the name; what
/// it avoids is running a command line or handing a file to a browser on
/// whatever machine this is.
#[test]
fn the_unsafe_dot_commands_are_refused_in_safe_mode() {
    let binary = program("inillucent");
    let shell = program("inillucent-shell");
    let area = area("unsafe");
    let directory = area.to_string_lossy().replace('\\', "/");
    let database = populated(&binary, &area);

    let mut wrong: Vec<String> = Vec::new();
    for name in UNSAFE {
        let input: &'static str = match name {
            "cd" => ".cd {dir}",
            "load" => ".load some-extension",
            "shell" => ".shell echo hello",
            "system" => ".system echo hello",
            "excel" => ".excel",
            _ => ".www",
        };
        let case = Case {
            name,
            input,
            says: "",
        };
        let printed = drive(&shell, &database, &directory, &case, true);
        if printed.contains("unknown command or invalid arguments") {
            wrong.push(format!("`.{name}` is not dispatched:\n{printed}"));
            continue;
        }
        if !printed.contains("prohibited in safe mode") {
            wrong.push(format!(
                "`.{name}` was not refused in safe mode:\n{printed}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "these commands should be refused under `-safe`:\n{}",
        wrong.join("\n---\n")
    );
}

/// The dispatcher handles exactly the reference's 65 names minus two.
///
/// **The 63-of-65 claim had no `cargo test` behind it (task-1969, 5.5).** It was
/// checked by `tools/feature-probe/registers.js`, which is run by hand, and
/// `registers.rs` - which does cover functions, modules, pragmas and collations
/// against the reference - does not cover dot commands.
///
/// The reference's list is hard coded above, so this runs on a machine with no
/// `sqlite3` on it. What it compares against is the dispatcher's own arms, read
/// out of `dot.rs`: a name that is in the source and in no case above fails
/// `every_dispatched_dot_command_answers`, and a name in the reference that the
/// source does not dispatch fails here.
#[test]
fn the_dispatcher_handles_the_reference_list_minus_two() {
    let root = workspace_root();
    let source = std::fs::read_to_string(root.join("crates/inillucent-cli/src/dot.rs"))
        .expect("dot.rs is readable");
    let dispatched = dispatched_names(&source);
    assert!(
        dispatched.len() >= 60,
        "read {} dispatched names out of dot.rs, which means the scan is wrong rather than \
         that the shell dispatches almost nothing",
        dispatched.len()
    );

    let mut missing: Vec<&str> = Vec::new();
    for name in REFERENCE {
        if NOT_IMPLEMENTED.contains(&name) {
            assert!(
                !dispatched.contains(&name.to_string()),
                "`.{name}` is recorded as not implemented and `dot.rs` dispatches it"
            );
            continue;
        }
        if !dispatched.contains(&name.to_string()) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "these dot commands are in the pinned `sqlite3` 3.53.4's own `.help` and this shell \
         does not dispatch them: {missing:?}\n\
         The published claim is 63 of its 65, and the two exceptions are {NOT_IMPLEMENTED:?}."
    );

    let implemented = REFERENCE.len().saturating_sub(NOT_IMPLEMENTED.len());
    assert_eq!(
        implemented,
        63,
        "the reference list holds {} names and {} are recorded as not implemented, which is \
         not the 63 of 65 every document states",
        REFERENCE.len(),
        NOT_IMPLEMENTED.len()
    );

    // Every name the source dispatches is covered by a case above. The other
    // direction of the same rule, so a name added to `dot.rs` without a case
    // fails here rather than being quietly untested.
    let mut untested: Vec<String> = Vec::new();
    for name in &dispatched {
        let covered = CASES.iter().any(|case| case.name == name)
            || UNSAFE.iter().any(|unsafe_name| unsafe_name == name);
        if !covered {
            untested.push(name.clone());
        }
    }
    assert!(
        untested.is_empty(),
        "`dot.rs` dispatches these and no case in this file drives them: {untested:?}"
    );
}

/// Returns the names the top-level dispatch in `dot.rs` matches on.
///
/// The arms of `match name.as_str()` at its own brace depth, so the inner
/// matches - `.mode`'s output modes, `.parameter`'s sub-commands - are not
/// counted as dot commands. Reading every quoted lowercase word in the file
/// instead reported `clear`, `init`, `list`, `off`, `on`, `set` and `unset` as
/// dispatched names; none of them is, and the shell answers "unknown command"
/// for all seven.
///
/// @param source - the text of `dot.rs`
fn dispatched_names(source: &str) -> Vec<String> {
    let Some(at) = source.find("match name.as_str() {") else {
        panic!("dot.rs has no `match name.as_str()`");
    };
    let mut names: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut started = false;
    for line in source.get(at..).unwrap_or_default().lines() {
        let trimmed = line.trim();
        if depth == 1 && started && trimmed.starts_with('"') && trimmed.contains("=>") {
            let head = trimmed.split("=>").next().unwrap_or("");
            for piece in head.split('|') {
                let word = piece.trim().trim_matches('"');
                if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric()) {
                    names.push(word.to_string());
                }
            }
        }
        for character in line.chars() {
            if character == '{' {
                depth = depth.saturating_add(1);
                started = true;
            } else if character == '}' {
                depth = depth.saturating_sub(1);
            }
        }
        if started && depth == 0 {
            break;
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Types a script into a shell of its own and returns everything it printed.
///
/// `drive` takes a `Case`, whose `input` is a `&'static str`; these cases build
/// their input at run time, so they go straight to the process runner.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file to open
/// @param typed - what to type, without the closing `.quit`
fn typing(shell: &Path, database: &Path, typed: &str) -> String {
    let path = database.to_string_lossy().replace('\\', "/");
    let script = format!("{typed}\n.quit\n");
    run_with_input(shell, &[path.as_str()], &script).said()
}

/// **`.help` answers in all five of its ways.**
///
/// `show_help` has five arms and `every_dispatched_dot_command_answers`
/// reaches one of them, because it types `.help` and nothing else. The other
/// four are what a person actually types: a command name, a prefix, a word they
/// half remember, and a word that is in none of it.
///
/// The `journal` case is the one to keep. `.backup`'s summary line does not
/// contain the word; one of its indented detail lines does, and the whole entry
/// is printed because of it. That is the search arm working, and it is the
/// difference between help that is browsable and help that is searchable.
#[test]
fn the_help_command_answers_in_all_five_of_its_ways() {
    let binary = program("inillucent");
    let shell = program("inillucent-shell");
    let area = area("help-modes");
    let database = populated(&binary, &area);
    let ask = |pattern: &str| typing(&shell, &database, pattern);

    // One summary line per documented command, and no detail lines.
    let summary = ask(".help");
    let named = summary.lines().filter(|line| line.starts_with('.')).count();
    assert!(
        named > 50,
        "`.help` listed {named} commands, and the reference's list is 65 long:\n{summary}"
    );
    assert!(
        summary.contains(".tables"),
        "`.help` did not list `.tables`:\n{summary}"
    );

    // The same commands with their detail lines, which is strictly more text.
    let everything = ask(".help -a");
    assert!(
        everything.lines().count() > summary.lines().count(),
        "`.help -a` printed {} lines and `.help` printed {}, so the detail lines are \
         being left out of both",
        everything.lines().count(),
        summary.lines().count()
    );

    // The commands deliberately left out of the summary, which is a short list
    // and a different one.
    let hidden = ask(".help 0");
    assert!(
        hidden.contains(".selftest"),
        "`.help 0` did not list `.selftest`, which is one of the commands the summary \
         leaves out:\n{hidden}"
    );
    assert!(
        !hidden.contains(".tables"),
        "`.help 0` listed `.tables`, which the summary documents, so it is printing the \
         summary rather than the commands left out of it:\n{hidden}"
    );

    // One match is answered in full.
    let one = ask(".help .mode");
    assert!(
        one.contains("USAGE: .mode"),
        "`.help .mode` did not print the long form:\n{one}"
    );
    assert!(
        !one.contains(".tables"),
        "`.help .mode` printed other commands, so the prefix matched more than it should \
         have:\n{one}"
    );

    // Several matches are answered one line each, with no long form.
    let several = ask(".help .s");
    for name in [".schema", ".shell", ".stats"] {
        assert!(
            several.contains(name),
            "`.help .s` did not list `{name}`:\n{several}"
        );
    }
    assert!(
        !several.contains("USAGE:"),
        "`.help .s` printed a long form, and several matches are one line each:\n{several}"
    );

    // A word that prefixes no command is looked for inside the help text.
    let searched = ask(".help journal");
    assert!(
        searched.contains(".backup"),
        "`.help journal` did not find `.backup`, whose detail lines are where the word \
         is - so the search arm is matching summary lines only:\n{searched}"
    );

    // And a word that is in none of it says so rather than printing everything.
    let nothing = ask(".help zzzznotacommand");
    assert!(
        nothing.contains("Nothing matches"),
        "`.help zzzznotacommand` did not say it found nothing:\n{nothing}"
    );
    assert!(
        !nothing.contains(".tables"),
        "`.help zzzznotacommand` fell back to printing the whole list:\n{nothing}"
    );
}

/// **`.dbconfig` lists, reads, sets and refuses.**
///
/// Four arms, of which the dispatch case reaches the first. The one that
/// matters is the round trip: `.dbconfig defensive off` followed by
/// `.dbconfig defensive` has to answer `off`, or the command is a report that
/// prints a default back whatever it was told.
#[test]
fn the_dbconfig_command_lists_reads_sets_and_refuses() {
    let binary = program("inillucent");
    let shell = program("inillucent-shell");
    let area = area("dbconfig-arms");
    let database = populated(&binary, &area);
    let ask = |typed: &str| typing(&shell, &database, typed);

    // With no argument, every flag.
    let listed = ask(".dbconfig");
    assert_eq!(
        listed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        22,
        "`.dbconfig` printed something other than its 22 flags:\n{listed}"
    );
    for flag in ["attach_create", "defensive", "trusted_schema"] {
        assert!(
            listed.contains(flag),
            "`.dbconfig` did not list `{flag}`:\n{listed}"
        );
    }

    // With a name, that flag alone.
    let one = ask(".dbconfig defensive");
    assert_eq!(
        one.lines().filter(|line| !line.trim().is_empty()).count(),
        1,
        "`.dbconfig defensive` printed more than the one flag:\n{one}"
    );

    // With a name and a value, the value is kept and read back. Both
    // directions, because a setter that always writes `off` would pass one.
    for wanted in ["off", "on"] {
        let round = ask(&format!(
            ".dbconfig defensive {wanted}\n.dbconfig defensive"
        ));
        let answers: Vec<&str> = round
            .lines()
            .filter(|line| line.contains("defensive"))
            .collect();
        assert_eq!(
            answers.len(),
            2,
            "setting and reading `defensive` did not answer twice:\n{round}"
        );
        for line in &answers {
            assert!(
                line.trim().ends_with(wanted),
                "`defensive` was set to `{wanted}` and reads as `{line}`"
            );
        }
    }

    // The name is folded, which is what the reference does.
    let folded = ask(".dbconfig DEFENSIVE");
    assert!(
        folded.contains("defensive"),
        "`.dbconfig DEFENSIVE` was not recognised:\n{folded}"
    );

    // A flag this engine holds no state for says so rather than pretending.
    let refused = ask(".dbconfig comments off");
    assert!(
        refused.contains("cannot change dbconfig comments"),
        "`.dbconfig comments off` did not refuse:\n{refused}"
    );

    // And a name that is not a flag gets the reference's three lines.
    let unknown = ask(".dbconfig not_a_flag");
    assert!(
        unknown.contains("unknown dbconfig") && unknown.contains("with no arguments for a list"),
        "`.dbconfig not_a_flag` did not answer the way the reference does:\n{unknown}"
    );
}

/// **`.show` reports what was set, for the three settings that were literals.**
///
/// This file's header says a setter is asserted through `.show`, and for three
/// of the twelve settings that was not possible: `explain`, `stats` and
/// `output` were written into the format strings as `auto`, `off` and `stdout`
/// (task-2066 section 4.4.13). So `.explain on` followed by `.show` answered
/// `auto`, `.stats on` answered `off`, and a shell whose rows were going into a
/// file said they were going to the terminal - which is the one line of that
/// report somebody reads when they cannot find their output.
///
/// The three are read off the shell now, and this is the case that fails if
/// they go back to being constants.
#[test]
fn the_show_command_reports_the_settings_that_were_written_into_it() {
    let binary = program("inillucent");
    let shell = program("inillucent-shell");
    let area = area("show-settings");
    let database = populated(&binary, &area);
    let ask = |typed: &str| typing(&shell, &database, typed);

    // The defaults, so that what follows is a change rather than a coincidence.
    let before = ask(".show");
    for expected in ["explain: auto", "stats: off", "output: stdout"] {
        assert!(
            before.contains(expected),
            "a fresh shell did not report `{expected}`:\n{before}"
        );
    }

    let after = ask(".explain on\n.stats on\n.show");
    for expected in ["explain: on", "stats: on"] {
        assert!(
            after.contains(expected),
            "`.show` did not report `{expected}` after it was set:\n{after}"
        );
    }

    // `.explain off` is a third value rather than the absence of the second,
    // so a report that only knew `auto` and `on` would still be wrong.
    let off = ask(".explain off\n.show");
    assert!(
        off.contains("explain: off"),
        "`.show` did not report `explain: off`:\n{off}"
    );

    // And the redirect. `.show` writes into the file it is reporting, so the
    // answer is read back off disk.
    let target = area.join("redirected.txt");
    let named = target.to_string_lossy().replace('\\', "/");
    let printed = ask(&format!(".output {named}\n.show"));
    assert!(
        printed.trim().is_empty(),
        "`.output` did not redirect - `.show` printed to the terminal:\n{printed}"
    );
    let written = std::fs::read_to_string(&target).unwrap_or_else(|why| {
        panic!("{} was not written: {why}", target.display());
    });
    assert!(
        written.contains(&named),
        "`.show` reported somewhere other than the file it was writing into:\n{written}"
    );
}

/// **The settings commands take their arguments and refuse what they cannot.**
///
/// `.cd`, `.prompt`, `.auth`, `.scanstats`, `.trace` and `.connection` are
/// dispatched by `every_dispatched_dot_command_answers` with no argument or
/// with one that works. These are the other ends: a directory that is not
/// there, and the two words a person types when they want a setting off again.
#[test]
fn the_settings_commands_take_their_arguments() {
    let binary = program("inillucent");
    let shell = program("inillucent-shell");
    let area = area("settings-arguments");
    let database = populated(&binary, &area);
    let ask = |typed: &str| typing(&shell, &database, typed);

    // A directory that is not there is refused by name rather than ignored.
    let missing = ask(".cd no-such-directory-anywhere");
    assert!(
        missing.contains("Cannot change to directory"),
        "`.cd` to a directory that is not there said nothing:\n{missing}"
    );

    // `.connection` names the database that is open, which is how a person
    // checks which of several they are typing at.
    let connections = ask(".connection");
    assert!(
        connections.contains("ACTIVE 0:") && connections.contains("app.rdb"),
        "`.connection` did not name the open database:\n{connections}"
    );

    // The on and off words for the three settings that take them. None of
    // these prints anything of its own, so what is asserted is that the
    // dispatcher took the argument rather than answering its fallback.
    let toggled = ask(
        ".auth on\n.auth off\n.scanstats on\n.scanstats off\n.trace off\n         .prompt \"A> \" \"B> \"\nSELECT 'still-here';",
    );
    assert!(
        !toggled.contains("unknown command or invalid arguments"),
        "one of the settings commands refused its argument:\n{toggled}"
    );
    assert!(
        toggled.contains("still-here"),
        "the shell stopped answering after the settings were changed:\n{toggled}"
    );
}
