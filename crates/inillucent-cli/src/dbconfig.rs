//! `.dbconfig`: the connection flags `sqlite3_db_config` sets.
//!
//! Invariant: a flag this shell lists is a flag this engine acts on, or it is a
//! flag whose value cannot be moved and says so. There is no third state where
//! `.dbconfig X on` prints `on` and nothing changes - that is the failure the
//! pragma surface was rebuilt to remove (see `inillucent_engine::pragma`), and
//! a connection flag is no different from a pragma in what a caller may
//! conclude from it.
//!
//! ## Why the shell has this at all
//!
//! `defensive` is the reason. The reference's shell turns it **on** by default,
//! and the visible consequence is that `PRAGMA journal_mode = OFF` is refused:
//! it comes back reporting whatever mode was already in force. Without the flag
//! this engine honoured `OFF` and the two shells disagreed on a statement
//! nobody would think to check. `.recover` writes `.dbconfig defensive off` as
//! its first line for the same reason - the script it emits writes
//! `sqlite_schema` directly, which defensive mode forbids.
//!
//! The other flags are listed because the listing is the command: `.dbconfig`
//! with no argument prints all of them, and a listing that showed a different
//! set from the reference's would be a different command wearing the same name.

use crate::shell::Shell;

/// One connection flag: its name, and what it reads as by default.
///
/// The defaults are the reference's own, taken from a `.dbconfig` on a fresh
/// database rather than from its documentation.
struct Flag {
    /// The name `.dbconfig` prints and accepts.
    name: &'static str,
    /// What it reads on a connection nobody has changed.
    default: Value,
    /// Whether this engine can move it.
    settable: bool,
}

/// What a flag reads as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    /// A boolean flag, printed as `on` or `off`.
    Switch(bool),
    /// A numeric flag, printed as itself.
    Number(i64),
}

/// The flags, in the order the reference prints them.
///
/// Alphabetical, which is the order `sqlite3_db_config`'s own table is in.
const FLAGS: [Flag; 22] = [
    Flag {
        name: "attach_create",
        default: Value::Switch(true),
        settable: false,
    },
    Flag {
        name: "attach_write",
        default: Value::Switch(true),
        settable: false,
    },
    Flag {
        name: "comments",
        default: Value::Switch(true),
        settable: false,
    },
    Flag {
        name: "defensive",
        default: Value::Switch(true),
        settable: true,
    },
    Flag {
        name: "dqs_ddl",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "dqs_dml",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "enable_fkey",
        default: Value::Switch(false),
        settable: true,
    },
    Flag {
        name: "enable_qpsg",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "enable_trigger",
        default: Value::Switch(true),
        settable: false,
    },
    Flag {
        name: "enable_view",
        default: Value::Switch(true),
        settable: false,
    },
    Flag {
        name: "fts3_tokenizer",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "fp_digits",
        default: Value::Number(17),
        settable: false,
    },
    Flag {
        name: "legacy_alter_table",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "legacy_file_format",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "load_extension",
        default: Value::Switch(true),
        settable: false,
    },
    Flag {
        name: "no_ckpt_on_close",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "reset_database",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "reverse_scanorder",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "stmt_scanstatus",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "trigger_eqp",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "trusted_schema",
        default: Value::Switch(false),
        settable: false,
    },
    Flag {
        name: "writable_schema",
        default: Value::Switch(false),
        settable: true,
    },
];

/// How wide the name column is.
///
/// The reference right-aligns each name in nineteen characters and then prints
/// a space and the value, which is what makes the listing a column.
const NAME_WIDTH: usize = 19;

/// `.dbconfig ?NAME? ?on|off?`: read or set one connection flag.
///
/// With no argument every flag is listed; with a name, that one; with a name
/// and a value, that one is set and then printed, which is the reference's own
/// acknowledgement.
///
/// @param shell - the shell
/// @param arguments - the words after the command
pub fn dbconfig(shell: &mut Shell, arguments: &[&str]) {
    let Some(name) = arguments.first().copied() else {
        for flag in &FLAGS {
            let line = render(flag.name, current(shell, flag));
            shell.say(&line);
        }
        return;
    };
    let folded = name.to_ascii_lowercase();
    let Some(flag) = FLAGS.iter().find(|held| held.name == folded) else {
        unknown(shell, arguments);
        return;
    };
    if let Some(word) = arguments.get(1) {
        let wanted = crate::dot::truthy(Some(word));
        if !apply(shell, flag, wanted) {
            shell.complain(&format!(
                "Error: this build cannot change dbconfig {}",
                flag.name
            ));
            return;
        }
    }
    let line = render(flag.name, current(shell, flag));
    shell.say(&line);
}

/// Reports a name that is not one of the flags, the reference's way.
///
/// Three lines: the command as typed, a caret under the name that was not
/// recognised, and where to look. The caret's column is the width of
/// `.dbconfig ` because that is where the first argument starts.
///
/// @param shell - the shell
/// @param arguments - the words after the command
fn unknown(shell: &mut Shell, arguments: &[&str]) {
    /// What `.dbconfig ` occupies, which is where the argument begins.
    const COMMAND: usize = 10;

    let line = shell.line;
    let typed = format!(".dbconfig {}", arguments.join(" "));
    shell.complain(&format!("line {line}: {typed}"));
    shell.complain(&format!(
        "line {line}: {}^--- unknown dbconfig",
        " ".repeat(COMMAND)
    ));
    shell.complain(&format!(
        "line {line}: Enter \".dbconfig\" with no arguments for a list"
    ));
}

/// Renders one flag's line.
///
/// @param name - the flag's name
/// @param value - what it reads as
fn render(name: &str, value: Value) -> String {
    let text = match value {
        Value::Switch(true) => "on".to_string(),
        Value::Switch(false) => "off".to_string(),
        Value::Number(number) => number.to_string(),
    };
    format!("{name:>NAME_WIDTH$} {text}")
}

/// Returns what a flag reads as right now.
///
/// The four the engine holds are asked of it; the rest have not moved from
/// their defaults, because nothing can move them.
///
/// @param shell - the shell
/// @param flag - the flag
fn current(shell: &mut Shell, flag: &Flag) -> Value {
    match flag.name {
        "defensive" => Value::Switch(shell.defensive),
        "enable_fkey" => Value::Switch(shell.boolean_pragma("foreign_keys")),
        "writable_schema" => Value::Switch(shell.boolean_pragma("writable_schema")),
        _ => flag.default,
    }
}

/// Sets a flag, reporting whether this engine could.
///
/// @param shell - the shell
/// @param flag - the flag
/// @param wanted - the value asked for
fn apply(shell: &mut Shell, flag: &Flag, wanted: bool) -> bool {
    if !flag.settable {
        // Settable to what it already is, which is what every "reported" value
        // in this workspace accepts: asking for the state you are in is not a
        // change and refusing it would be pedantry.
        return current(shell, flag) == Value::Switch(wanted);
    }
    match flag.name {
        "defensive" => {
            shell.defensive = wanted;
            shell.set_defensive(wanted)
        }
        "enable_fkey" => shell.set_boolean_pragma("foreign_keys", wanted),
        "writable_schema" => shell.set_boolean_pragma("writable_schema", wanted),
        _ => false,
    }
}
