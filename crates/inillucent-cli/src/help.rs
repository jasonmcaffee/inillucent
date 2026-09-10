//! The `.help` text, ported from the reference shell's own tables.
//!
//! Invariant: **every command described here exists, and is described in the
//! reference's own words.** Help is the one part of a shell a person reads
//! instead of testing, so an entry copied for a command that was never built
//! would be believed; and an entry reworded would make `.help .mode` a
//! different page from the one it is compared against.
//!
//! Two tables and one search, all three of them `shell.c`'s. `HELP` is
//! `azHelp[]`: one entry per dot-command, its first line the summary the
//! bare `.help` prints and the indented lines after it the detail a search
//! that matched exactly one command prints. An entry beginning with `,`
//! rather than `.` is an **undocumented** command - it is left out of the
//! summary listing and out of `.help -all`, and `.help 0` is the only thing
//! that shows it. `USAGE` is `aUsage[]`, the long-form text four commands
//! carry instead of their detail lines.
//!
//! **The entries are the reference's own words, and only for the commands
//! this shell actually has.** Help that described a command that is not here
//! would be a worse answer than no help at all, and the reference's wording
//! is the point: `.help .mode` is compared byte for byte.

/// Every dot-command this shell has, as `azHelp[]` holds them.
pub const HELP: &[&str] = &[
    ".archive ...             Manage SQL archives",
    ".auth ON|OFF             Show authorizer callbacks",
    ".backup ?DB? FILE        Backup DB (default \"main\") to FILE",
    "   Options:",
    "       --append            Use the appendvfs",
    "       --async             Write to FILE without journal and fsync()",
    ".bail on|off             Stop after hitting an error.  Default OFF",
    ".cd DIRECTORY            Change the working directory to DIRECTORY",
    ".changes on|off          Show number of rows changed by SQL",
    ".check OPTIONS ...       Verify the results of a .testcase",
    ".clone NEWDB             Clone data into NEWDB from the existing database",
    ".connection [close] [#]  Open or close an auxiliary database connection",
    ".crlf ?on|off?           Whether or not to use \\r\\n line endings",
    ".databases               List names and files of attached databases",
    ".dbconfig ?op? ?val?     List or change sqlite3_db_config() options",
    ".dbinfo ?DB?             Show status information about the database",
    ".dbtotxt                 Hex dump of the database file",
    ".dump ?OBJECTS?          Render database content as SQL",
    "   Options:",
    "     --data-only            Output only INSERT statements",
    "     --newlines             Allow unescaped newline characters in output",
    "     --nosys                Omit system tables (ex: \"sqlite_stat1\")",
    "     --preserve-rowids      Include ROWID values in the output",
    "   OBJECTS is a LIKE pattern for tables, indexes, triggers or views to dump",
    "   Additional LIKE patterns can be given in subsequent arguments",
    ".echo on|off             Turn command echo on or off",
    ".eqp on|off|full|...     Enable or disable automatic EXPLAIN QUERY PLAN",
    "   Other Modes:",
    "      test                  Show raw EXPLAIN QUERY PLAN output",
    "      trace                 Like \"full\" but enable \"PRAGMA vdbe_trace\"",
    "      trigger               Like \"full\" but also show trigger bytecode",
    ".excel                   Display the output of next command in spreadsheet",
    ".exit ?CODE?             Exit this program with return-code CODE",
    ".explain ?on|off|auto?   Change the EXPLAIN formatting mode.  Default: auto",
    ".filectrl CMD ...        Run various sqlite3_file_control() operations",
    ".fullschema ?--indent?   Show schema and the content of sqlite_stat tables",
    ".headers on|off          Turn display of headers on or off",
    ".help ?-all? ?PATTERN?   Show help text for PATTERN",
    ".import FILE TABLE       Import data from FILE into TABLE",
    ".imposter INDEX TABLE    Create imposter table TABLE on index INDEX",
    ".indexes ?PATTERN?       Show names of indexes matching PATTERN",
    "   -a|--all                Also show system-generated indexes",
    "   --expr                  Show only expression indexes",
    "   --sys                   Show only system-generated indexes",
    ".intck ?STEPS_PER_UNLOCK?  Run an incremental integrity check on the db",
    ".limit ?LIMIT? ?VAL?     Display or change the value of an SQLITE_LIMIT",
    ".lint OPTIONS            Report potential schema issues.",
    "     Options:",
    "        fkey-indexes     Find missing foreign key indexes",
    ".load FILE ?ENTRY?       Load an extension library",
    ".log FILE|on|off         Turn logging on or off.  FILE can be stderr/stdout",
    ".mode ?MODE? ?OPTIONS?   Set output mode",
    ".nonce STRING            Suspend safe mode for one command if nonce matches",
    ".nullvalue STRING        Use STRING in place of NULL values",
    ".once ?OPTIONS? ?FILE?   Output for the next SQL command only to FILE",
    ".open ?OPTIONS? ?FILE?   Close existing database and reopen FILE",
    "     Options:",
    "        --append        Use appendvfs to append database to the end of FILE",
    "        --deserialize   Load into memory using sqlite3_deserialize()",
    "        --hexdb         Load the output of \"dbtotxt\" as an in-memory db",
    "        --ifexist       Only open if FILE already exists",
    "        --maxsize N     Maximum size for --hexdb or --deserialized database",
    "        --new           Initialize FILE to an empty database",
    "        --normal        FILE is an ordinary SQLite database",
    "        --nofollow      Do not follow symbolic links",
    "        --readonly      Open FILE readonly",
    "        --zip           FILE is a ZIP archive",
    ".output ?FILE?           Send output to FILE or stdout if FILE is omitted",
    ".parameter CMD ...       Manage SQL parameter bindings",
    "   clear                   Erase all bindings",
    "   init                    Initialize the TEMP table that holds bindings",
    "   list                    List the current parameter bindings",
    "   set PARAMETER VALUE     Given SQL parameter PARAMETER a value of VALUE",
    "                           PARAMETER should start with one of: $ : @ ?",
    "   unset PARAMETER         Remove PARAMETER from the binding table",
    ".print STRING...         Print literal STRING",
    ".progress N              Invoke progress handler after every N opcodes",
    ".prompt MAIN CONTINUE    Replace the standard prompts",
    ".quit                    Stop interpreting input stream, exit if primary.",
    ".read FILE               Read input from FILE or command output",
    "    If FILE begins with \"|\", it is a command that generates the input.",
    ".recover                 Recover as much data as possible from corrupt db.",
    "   --ignore-freelist        Ignore pages that appear to be on db freelist",
    "   --lost-and-found TABLE   Alternative name for the lost-and-found table",
    "   --no-rowids              Do not attempt to recover rowid values",
    "                            that are not also INTEGER PRIMARY KEYs",
    ".restore ?DB? FILE       Restore content of DB (default \"main\") from FILE",
    ".save ?OPTIONS? FILE     Write database to FILE (an alias for .backup ...)",
    ".scanstats on|off|est    Turn sqlite3_stmt_scanstatus() metrics on or off",
    ".schema ?PATTERN?        Show the CREATE statements matching PATTERN",
    "   Options:",
    "      --indent             Try to pretty-print the schema",
    "      --nosys              Omit objects whose names start with \"sqlite_\"",
    ",selftest ?OPTIONS?      Run tests defined in the SELFTEST table",
    "    Options:",
    "       --init               Create a new SELFTEST table",
    "       -v                   Verbose output",
    ",separator COL ?ROW?     Change the column and row separators",
    ".sha3sum ...             Compute a SHA3 hash of database content",
    "    Options:",
    "      --schema              Also hash the sqlite_schema table",
    "      --sha3-224            Use the sha3-224 algorithm",
    "      --sha3-256            Use the sha3-256 algorithm (default)",
    "      --sha3-384            Use the sha3-384 algorithm",
    "      --sha3-512            Use the sha3-512 algorithm",
    "    Any other argument is a LIKE pattern for tables to hash",
    ",show                    Show the current values for various settings",
    ".shell CMD ARGS...       Run CMD ARGS... in a system shell",
    ".stats ?ARG?             Show stats or turn stats on or off",
    ".system CMD ARGS...      Run CMD ARGS... in a system shell",
    ".tables ?TABLE?          List names of tables matching LIKE pattern TABLE",
    ".testcase NAME           Begin a test case.",
    ".timeout MS              Try opening locked tables for MS milliseconds",
    ".timer on|off|once       Turn SQL timer on or off.",
    ".trace ?OPTIONS?         Output each SQL statement as it is run",
    ".version                 Show source, library and compiler versions",
    ".vfsinfo ?AUX?           Information about the top-level VFS",
    ".vfslist                 List all available VFSes",
    ".vfsname ?AUX?           Print the name of the VFS stack",
    ",width NUM1 NUM2 ...     Set minimum column widths for columnar output",
    "     Negative values right-justify",
    ".www                     Display output of the next command in web browser",
];

/// The long-form usage text, as `aUsage[]` holds it.
pub const USAGE: &[(&str, &str)] = &[
    (
        ".import",
        "USAGE: .import [OPTIONS] FILE TABLE\n\nImport CSV or similar text from FILE into TABLE.  If TABLE does\nnot exist, it is created using the first row of FILE as the column\nnames.  If FILE begins with \"|\" then it is a command that is run\nand the output from the command is used as the input data.  If\nFILE begins with \"<<\" followed by a label, then content is read from\nthe script until the first line that matches the label.\n\nThe content of FILE is interpreted using RFC-4180 (\"CSV\") quoting\nrules unless the current mode is \"ascii\" or \"tabs\" or unless one\nthe --ascii option is used.\n\nThe column and row separators must be single ASCII characters.  If\nmultiple characters or a Unicode character are specified for the\nseparators, then only the first byte of the separator is used.  Except,\nif the row separator is \\n and the mode is not --ascii, then \\r\\n is\nunderstood as a row separator too.\n\nOptions:\n  --ascii         Do not use RFC-4180 quoting.  Use \\037 and \\036\n                  as column and row separators on input, unless other\n                  delimiters are specified using --colsep and/or --rowsep\n  --colsep CHAR   Use CHAR as the column separator.\n  --csv           Input is standard RFC-4180 CSV.\n  --esc CHAR      Use CHAR as an escape character in unquoted CSV inputs.\n  --qesc CHAR     Use CHAR as an escape character in quoted CSV inputs.\n  --rowsep CHAR   Use CHAR as the row separator.\n  --schema S      When creating TABLE, put it in schema S\n  --skip N        Ignore the first N rows of input\n  -v              Verbose mode\n",
    ),
    (
        ".mode",
        "USAGE: .mode [MODE] [OPTIONS]\n\nChange the output mode to MODE and/or apply OPTIONS to the output mode.\nArguments are processed from left to right.  If no arguments, show the\ncurrent output mode and relevant options.\n\nOptions:\n  --align STRING           Set the alignment of text in columnar modes\n                           String consists of characters 'L', 'C', 'R'\n                           meaning \"left\", \"centered\", and \"right\", with\n                           one letter per column starting from the left.\n                           Unspecified alignment defaults to 'L'.\n  --blob-quote ARG         ARG can be \"auto\", \"text\", \"sql\", \"hex\", \"tcl\",\n                           \"json\", or \"size\".  Default is \"auto\".\n  --border on|off          Show outer border on \"box\" and \"table\" modes.\n  --charlimit N            Set the maximum number of output characters to\n                           show for any single SQL value to N. Longer values\n                           truncated. Zero means \"no limit\".\n  --colsep STRING          Use STRING as the column separator\n  --escape ESC             Enable/disable escaping of control characters\n                           found in the output. ESC can be \"off\", \"ascii\",\n                           or \"symbol\".\n  --linelimit N            Set the maximum number of output lines to show for\n                           any single SQL value to N. Longer values are\n                           truncated. Zero means \"no limit\". Only works\n                           in \"line\" mode and in columnar modes.\n  --limits L,C,T           Shorthand for \"--linelimit L --charlimit C\n                           --titlelimit T\". The \",T\" can be omitted in which\n                           case the --titlelimit is unchanged.  The argument\n                           can also be \"off\" to mean \"0,0,0\" or \"on\" to\n                           mean \"5,300,20\".\n  --list                   List available modes\n  --multiinsert N          In \"insert\" mode, put multiple rows on a single\n                           INSERT statement until the size exceeds N bytes.\n  --null STRING            Render SQL NULL values as the given string\n  --once                   Setting changes to the right are reverted after\n                           the next SQL command.\n  --quote ARG              Enable/disable quoting of text. ARG can be\n                           \"off\", \"on\", \"sql\", \"relaxed\", \"csv\", \"html\",\n                           \"tcl\", or \"json\". \"off\" means show the text as-is.\n                           \"on\" is an alias for \"sql\".\n  --reset                  Changes all mode settings back to their default.\n  --rowsep STRING          Use STRING as the row separator\n  --sw|--screenwidth N     Declare the screen width of the output device\n                           to be N characters.  An attempt may be made to\n                           wrap output text to fit within this limit. Zero\n                           means \"no limit\".  Or N can be \"auto\" to set the\n                           width automatically.\n  --tablename NAME         Set the name of the table for \"insert\" mode.\n  --tag NAME               Save mode to the left as NAME.\n  --textjsonb BOOLEAN      If enabled, JSONB text is displayed as text JSON.\n  --title ARG              Whether or not to show column headers, and if so\n                           how to encode them.  ARG can be \"off\", \"on\",\n                           \"sql\", \"csv\", \"html\", \"tcl\", or \"json\".\n  --titlelimit N           Limit the length of column titles to N characters.\n  -v|--verbose             Verbose output\n  --widths LIST            Set the columns widths for columnar modes. The\n                           argument is a list of integers, one for each\n                           column. A \"0\" width means use a dynamic width\n                           based on the actual width of data. If there are\n                           fewer entries in LIST than columns, \"0\" is used\n                           for the unspecified widths.\n  --wordwrap BOOLEAN       Enable/disable word wrapping\n  --wrap N                 Wrap columns wider than N characters\n  --ww                     Shorthand for \"--wordwrap on\"\n",
    ),
    (
        ".output",
        "USAGE: .output [OPTIONS] [FILE]\n\nBegin redirecting output to FILE.  Or if FILE is omitted, revert\nto sending output to the console.  If FILE begins with \"|\" then\nthe remainder of file is taken as a pipe and output is directed\ninto that pipe.  If FILE is \"memory\" then output is captured in an\ninternal memory buffer.  If FILE is \"off\" then output is redirected\ninto /dev/null or the equivalent.\n\nOptions:\n  --bom             Prepend a byte-order mark to the output\n  -e                Accumulate output in a temporary text file then\n                    launch a text editor when the redirection ends.\n  --error-prefix X  Use X as the left-margin prefix for error messages.\n                    Set to an empty string to restore the default.\n  --keep            Keep redirecting output to its current destination.\n                    Use this option in combination with --show or\n                    with --error-prefix when you do not want to stop\n                    a current redirection.\n  --plain           Use plain text rather than HTML tables with -w\n  --show            Show output text captured by .testcase or by\n                    redirecting to \"memory\".\n  -w                Show the output in a web browser.  Output is\n                    written into a temporary HTML file until the\n                    redirect ends, then the web browser is launched.\n                    Query results  are shown as HTML tables, unless\n                    the --plain is used too.\n  -x                Show the output in a spreadsheet.  Output is\n                    written to a temp file as CSV then the spreadsheet\n                    is launched when\n",
    ),
    (
        ".once",
        "USAGE: .once [OPTIONS] FILE ...\n\nWrite the output for the next line of SQL or the next dot-command into\nFILE.  If FILE begins with \"|\" then it is a program into which output\nis written. The FILE argument should be omitted if one of the -e, -w,\nor -x options is used.\n\nOptions:\n  -e                Capture output into a temporary file then bring up\n                    a text editor on that temporary file.\n  --plain           Use plain text rather than HTML tables with -w\n  -w                Capture output into an HTML file then bring up that\n                    file in a web browser\n  -x                Show the output in a spreadsheet.  Output is\n                    written to a temp file as CSV then the spreadsheet\n                    is launched when\n",
    ),
];

/// Returns the usage text whose command the pattern is a prefix of.
///
/// @param prefix - the command prefix, leading dot included
fn usage_for(prefix: &str) -> Option<&'static str> {
    USAGE
        .iter()
        .find(|(command, _)| command.starts_with(prefix))
        .map(|(_, text)| *text)
}

/// Returns whether an entry line is a continuation of the one before it.
///
/// @param line - the entry line
fn is_detail(line: &str) -> bool {
    line.starts_with(' ')
}

/// Writes the help a pattern asks for, and returns how many commands matched.
///
/// A port of `showHelp`, and the shape of the answer is the reference's:
///
/// - no pattern lists the summary line of every documented command;
/// - `-a`, `-all` or `--all` does the same, because a listing of every
///   command's detail would be the whole file;
/// - `0` lists the undocumented commands, in full;
/// - a pattern that is a prefix of exactly one command prints that command's
///   long-form usage if it has one and its detail lines if it does not;
/// - a prefix of several prints one summary line each;
/// - and a pattern that is a prefix of none is looked for *inside* the help
///   text, which is what makes `.help wal` find the commands that mention it.
///
/// @param pattern - what was written after `.help`, if anything
/// @param write - where each line goes
pub fn show_help(pattern: Option<&str>, write: &mut dyn FnMut(&str)) -> usize {
    let asked = pattern.unwrap_or("");
    if asked.is_empty() || asked == "-a" || asked == "-all" || asked == "--all" {
        return summarise(asked.is_empty(), write);
    }
    if asked == "0" {
        return undocumented(write);
    }
    let prefix = format!(".{}", asked.strip_prefix('.').unwrap_or(asked));
    let matched = by_prefix(&prefix, write);
    if matched > 0 {
        return matched;
    }
    containing(asked, write)
}

/// Lists one line per documented command.
///
/// @param summary_only - whether the detail lines are left out
/// @param write - where each line goes
fn summarise(summary_only: bool, write: &mut dyn FnMut(&str)) -> usize {
    let mut count = 0;
    for line in HELP {
        if line.starts_with(',') {
            continue;
        }
        if line.starts_with('.') {
            write(line);
            count += 1;
        } else if !summary_only {
            write(line);
        }
    }
    count
}

/// Lists the commands that are deliberately left out of the summary.
///
/// @param write - where each line goes
fn undocumented(write: &mut dyn FnMut(&str)) -> usize {
    let mut count = 0;
    let mut showing = false;
    for line in HELP {
        if line.starts_with('.') {
            showing = false;
        } else if let Some(rest) = line.strip_prefix(',') {
            showing = true;
            count += 1;
            write(&format!(".{rest}"));
        } else if showing {
            write(line);
        }
    }
    count
}

/// Writes the help for every command the prefix names.
///
/// **One match is answered in full and several are answered in one line
/// each**, which is the reference's rule and is why `.help .mode` prints a
/// page and `.help .s` prints a list.
///
/// @param prefix - the command prefix, leading dot included
/// @param write - where each line goes
fn by_prefix(prefix: &str, write: &mut dyn FnMut(&str)) -> usize {
    let mut hit: Option<usize> = None;
    let mut count = 0;
    for (index, line) in HELP.iter().enumerate() {
        if !line.starts_with(prefix) {
            continue;
        }
        if let Some(previous) = hit.and_then(|at| HELP.get(at)) {
            write(previous);
        }
        hit = Some(index);
        count += 1;
    }
    let Some(index) = hit else {
        return 0;
    };
    if count > 1 {
        if let Some(line) = HELP.get(index) {
            write(line);
        }
        return count;
    }
    if let Some(text) = usage_for(prefix) {
        for line in text.trim_end_matches('\n').split('\n') {
            write(line);
        }
        return count;
    }
    if let Some(line) = HELP.get(index) {
        write(line);
    }
    for line in HELP.iter().skip(index.saturating_add(1)) {
        if !is_detail(line) {
            break;
        }
        write(line);
    }
    count
}

/// Writes the help for every command whose text mentions the pattern.
///
/// The reference's last resort, and the one that makes `.help` searchable
/// rather than only browsable: the whole entry is printed for any command
/// whose summary *or detail* contains the text, case-insensitively.
///
/// @param pattern - the text to look for
/// @param write - where each line goes
fn containing(pattern: &str, write: &mut dyn FnMut(&str)) -> usize {
    let needle = pattern.to_ascii_lowercase();
    let mut count = 0;
    let mut start = 0usize;
    let mut index = 0usize;
    while let Some(line) = HELP.get(index) {
        if line.starts_with(',') {
            index = index.saturating_add(1);
            while HELP.get(index).is_some_and(|line| is_detail(line)) {
                index = index.saturating_add(1);
            }
            continue;
        }
        if line.starts_with('.') {
            start = index;
        }
        if line.to_ascii_lowercase().contains(&needle) {
            if let Some(head) = HELP.get(start) {
                write(head);
            }
            let mut detail = start.saturating_add(1);
            while let Some(line) = HELP.get(detail).filter(|line| is_detail(line)) {
                write(line);
                detail = detail.saturating_add(1);
            }
            index = detail;
            count += 1;
            continue;
        }
        index += 1;
    }
    count
}
