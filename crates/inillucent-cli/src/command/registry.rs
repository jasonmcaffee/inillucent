//! Every command, as data.
//!
//! Invariant: **this array is the only list.** `inillucent --help`,
//! `inillucent <verb>`'s argument parsing, `inillucent-mcp`'s `tools/list` and
//! every JSON Schema it publishes are derived from it, and
//! `command_parity.rs` fails the build if any of them stops agreeing.
//!
//! The descriptions are written for two readers at once. A person runs
//! `inillucent help query`; a 27B model reads the same sentence as a tool
//! description and has one attempt at getting the call right. That is why they
//! say what a parameter *is for* rather than restating its name, and why the
//! ones with a trap in them - `limit` cutting the rows handed back but not the
//! count, `params` being positional - say so.

use super::verbs;
use super::{Command, Kind, Param, Writes, DB, FORMAT, LIMIT};

/// The parameters `query` takes.
const QUERY_PARAMS: &[Param] = &[
    Param {
        name: "sql",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "The SQL to run. One statement. Use ?1, ?2 ... for values you pass in \
                      'params' rather than pasting them into the text.",
    },
    Param {
        name: "params",
        kind: Kind::Values,
        required: false,
        positional: false,
        description: "The values for ?1, ?2 ... in order, as a JSON array of strings, numbers, \
                      booleans or nulls. An array of numbers is a vector and \
                      {\"blob\":\"<hex>\"} is bytes. Binding is how you avoid quoting mistakes \
                      and SQL injection.",
    },
    Param {
        name: "params-file",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "A file holding the JSON array for 'params', or - for standard input. What \
                      a wrapper that spawns this binary uses: a command line has a length \
                      ceiling, about 32 KB on Windows, and a parameter past it fails outright.",
    },
    LIMIT,
    DB,
    FORMAT,
];

/// The parameters `exec` takes.
const EXEC_PARAMS: &[Param] = &[
    Param {
        name: "sql",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "One statement to run for its effect: INSERT, UPDATE, DELETE, CREATE, DROP, \
                      ALTER or a PRAGMA that sets something.",
    },
    Param {
        name: "params",
        kind: Kind::Values,
        required: false,
        positional: false,
        description: "The values for ?1, ?2 ... in order, as a JSON array. An array of numbers \
                      is a vector and {\"blob\":\"<hex>\"} is bytes.",
    },
    Param {
        name: "params-file",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "A file holding the JSON array for 'params', or - for standard input. What \
                      a wrapper that spawns this binary uses: a command line has a length \
                      ceiling, about 32 KB on Windows, and a parameter past it fails outright.",
    },
    DB,
    FORMAT,
];

/// The parameters `batch` takes.
const BATCH_PARAMS: &[Param] = &[
    Param {
        name: "sql",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "Several statements separated by semicolons. They run as one transaction: \
                      either all of them take effect or none of them do.",
    },
    DB,
    FORMAT,
];

/// The parameters `run` takes.
const RUN_PARAMS: &[Param] = &[
    Param {
        name: "input",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "Shell input, exactly as you would type it: SQL statements ending in a \
                      semicolon, and dot commands such as '.schema' or '.mode box' on their own \
                      lines. Everything the interactive shell can do is reachable here.",
    },
    DB,
];

/// The parameters `create` takes.
const CREATE_PARAMS: &[Param] = &[
    Param {
        name: "path",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "Where to make the new database file. It refuses a path that already \
                      exists rather than overwriting it.",
    },
    FORMAT,
];

/// The parameters a pattern-filtered listing takes.
const PATTERN_PARAMS: &[Param] = &[
    Param {
        name: "pattern",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "A LIKE pattern to filter the names by, such as 'user%'. Omit it for all \
                      of them.",
    },
    DB,
    FORMAT,
];

/// The parameters `schema` takes.
const SCHEMA_PARAMS: &[Param] = &[
    Param {
        name: "pattern",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "A LIKE pattern naming which objects to show. Omit it for the whole schema.",
    },
    Param {
        name: "indent",
        kind: Kind::Boolean,
        required: false,
        positional: false,
        description: "Pretty-print each CREATE statement over several lines instead of one.",
    },
    DB,
    FORMAT,
];

/// The parameters `describe` takes.
const DESCRIBE_PARAMS: &[Param] = &[
    Param {
        name: "table",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "The table or view to describe, by its exact name. Use 'tables' first if \
                      you are not sure what it is called.",
    },
    DB,
    FORMAT,
];

/// The parameters the database-only commands take.
const DB_ONLY: &[Param] = &[DB, FORMAT];

/// The parameters `explain` takes.
const EXPLAIN_PARAMS: &[Param] = &[
    Param {
        name: "sql",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "The statement whose plan you want. It is not run - this only shows which \
                      indexes and scans the planner chose.",
    },
    DB,
    FORMAT,
];

/// The parameters `import` takes.
const IMPORT_PARAMS: &[Param] = &[
    Param {
        name: "file",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "The delimited file to read.",
    },
    Param {
        name: "table",
        kind: Kind::Text,
        required: true,
        positional: false,
        description: "The table to load into. If it does not exist it is created, taking its \
                      column names from the file's first row.",
    },
    Param {
        name: "format",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "'csv' (the default, RFC 4180 quoting), 'tabs', or 'ascii' for \\037 and \
                      \\036 separated input.",
    },
    Param {
        name: "skip",
        kind: Kind::Integer,
        required: false,
        positional: false,
        description: "How many leading rows to ignore, for a file with a preamble above its \
                      header.",
    },
    DB,
];

/// The parameters `export` takes.
const EXPORT_PARAMS: &[Param] = &[
    Param {
        name: "table",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "The table to write out whole. Give this or 'sql', not both.",
    },
    Param {
        name: "sql",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "A query whose rows to write out, when you want less than a whole table.",
    },
    Param {
        name: "format",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "csv, json, tabs, markdown, insert, quote, line or html. Defaults to csv.",
    },
    Param {
        name: "out",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "A file to write to. Omitted, the rows come back in the result instead.",
    },
    DB,
];

/// The parameters `dump` takes.
const DUMP_PARAMS: &[Param] = &[
    Param {
        name: "objects",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "A LIKE pattern for the tables, indexes, triggers or views to dump. Omit \
                      it for the whole database.",
    },
    Param {
        name: "data_only",
        kind: Kind::Boolean,
        required: false,
        positional: false,
        description: "Write only the INSERT statements, leaving out the CREATE statements.",
    },
    DB,
];

/// The parameters `backup` takes.
const BACKUP_PARAMS: &[Param] = &[
    Param {
        name: "file",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "Where to write the copy.",
    },
    DB,
];

/// The parameters `restore` takes.
const RESTORE_PARAMS: &[Param] = &[
    Param {
        name: "file",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "The copy to read back. It replaces what is in the open database.",
    },
    DB,
];

/// The parameters `analyze` takes.
const ANALYZE_PARAMS: &[Param] = &[
    Param {
        name: "table",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "One table to gather statistics for. Omit it for every table.",
    },
    DB,
    FORMAT,
];

/// The parameters `migrate` takes.
const MIGRATE_PARAMS: &[Param] = &[
    Param {
        name: "source",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "The SQLite database file to read, or a postgres:// or mysql:// connection \
                      URL. The source is never written to. A connection URL holds a password, \
                      and an argument is visible in the process list for the whole run - so it \
                      may be left out and given in INILLUCENT_SOURCE_URL instead, or written as \
                      '-' to read one line from standard input.",
    },
    Param {
        name: "destination",
        kind: Kind::Text,
        required: true,
        positional: false,
        description: "The .rdb file to build. It refuses to overwrite an existing file.",
    },
    Param {
        name: "kind",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "'sqlite' (the default for a path), 'postgres' or 'mysql' (the default for \
                      a URL written with that scheme), or 'index' for a legacy retrieval index.",
    },
    Param {
        name: "batch",
        kind: Kind::Integer,
        required: false,
        positional: false,
        description: "Rows per destination transaction while copying from a server. Default \
                      10000. It changes how long the migration takes and nothing about what it \
                      produces.",
    },
    Param {
        name: "insecure-plaintext",
        kind: Kind::Boolean,
        required: false,
        positional: false,
        description: "Permit an unencrypted connection to a server. A migration to a host that \
                      is not a loopback address uses verified TLS, and refuses rather than \
                      falling back; this permits plaintext, and only together with \
                      sslmode=disable in the URL. Both are needed because either one alone is \
                      something people type without meaning it. The choice is recorded in the \
                      migration report.",
    },
];

/// The parameters `search` takes.
const SEARCH_PARAMS: &[Param] = &[
    Param {
        name: "query",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "What to search for, in FTS5 query syntax: bare words are ANDed, \
                      \"a phrase\" is quoted, and OR and NOT are available.",
    },
    Param {
        name: "table",
        kind: Kind::Text,
        required: true,
        positional: false,
        description: "The full-text table to search - one created with USING fts5(...) or \
                      USING inillucent_search(...).",
    },
    Param {
        name: "k",
        kind: Kind::Integer,
        required: false,
        positional: false,
        description: "How many results to return, best first. Defaults to 10.",
    },
    DB,
    FORMAT,
];

/// The parameters `vector-search` takes.
const VECTOR_PARAMS: &[Param] = &[
    Param {
        name: "table",
        kind: Kind::Text,
        required: true,
        positional: true,
        description: "The table holding the vectors.",
    },
    Param {
        name: "column",
        kind: Kind::Text,
        required: true,
        positional: false,
        description: "The VECTOR(N) column to measure against.",
    },
    Param {
        name: "vector",
        kind: Kind::Values,
        required: true,
        positional: false,
        description: "The query vector, as a JSON array of numbers with exactly N elements.",
    },
    Param {
        name: "k",
        kind: Kind::Integer,
        required: false,
        positional: false,
        description: "How many nearest rows to return. Defaults to 10.",
    },
    Param {
        name: "measure",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "'cos' for cosine distance (the default), 'l2' for Euclidean, or 'dot' \
                      for the inner product.",
    },
    DB,
    FORMAT,
];

/// The parameters `capabilities` takes.
const CAPABILITY_PARAMS: &[Param] = &[
    Param {
        name: "name",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "One capability to ask about, such as 'triggers'. Omit it for the whole \
                      table. A name that is not in the table answers no, not yes.",
    },
    FORMAT,
];

/// The parameters `help` takes.
const HELP_PARAMS: &[Param] = &[
    Param {
        name: "topic",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "A command to explain in full. Omit it for the list of every command.",
    },
    FORMAT,
];

/// The parameters `setup-embeddings` takes.
const SETUP_PARAMS: &[Param] = &[
    Param {
        name: "component",
        kind: Kind::Text,
        required: false,
        positional: true,
        description: "Which half to install: 'all' (the default), 'runtime' for the ONNX Runtime \
                      shared library on its own, or 'model' for the weights on their own.",
    },
    Param {
        name: "status",
        kind: Kind::Boolean,
        required: false,
        positional: false,
        description: "Say what is installed, where, and which residency profile is in force, and \
                      download nothing.",
    },
    Param {
        name: "residency",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "When the model is in memory: 'resident' keeps it for the life of the process, \
                      'on-demand' loads it per call and drops it, 'idle' or 'idle:90s' loads it on \
                      use and drops it after a quiet period. Recorded for this machine; \
                      INILLUCENT_EMBED_RESIDENCY overrides it for one process.",
    },
    Param {
        name: "gpu",
        kind: Kind::Boolean,
        required: false,
        positional: false,
        description: "Install the ONNX Runtime build carrying the CUDA execution provider, which \
                      exists for Windows and Linux on x86-64 only. It is a much larger download and \
                      it needs a CUDA install of its own to be usable.",
    },
    Param {
        name: "force",
        kind: Kind::Boolean,
        required: false,
        positional: false,
        description: "Fetch and install again even when the files are already there and their \
                      digests match.",
    },
    Param {
        name: "onnxruntime-version",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "The ONNX Runtime version to install. Defaults to the one this build pins a \
                      digest for; any other version is fetched and reported as unverified.",
    },
    Param {
        name: "dir",
        kind: Kind::Text,
        required: false,
        positional: false,
        description: "Install somewhere other than the per-user directory. INILLUCENT_HOME does the \
                      same thing for every command at once.",
    },
    FORMAT,
];

/// The parameters `version` takes.
const VERSION_PARAMS: &[Param] = &[FORMAT];

/// Every command inillucent has, in the order `help` lists them.
pub static COMMANDS: &[Command] = &[
    Command {
        name: "query",
        summary: "Run a SELECT and get its rows back.",
        detail: "Use this for anything that reads. The rows come back as a table, or as typed \
                 JSON with output=json. 'total' is exact even when 'limit' cut the rows handed \
                 back, because the engine materialises the whole result - so a limit of 10 on a \
                 million-row query still costs what the million rows cost. Put a LIMIT in the SQL \
                 itself when you cannot afford that, where the planner can act on it.",
        params: QUERY_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::query,
    },
    Command {
        name: "exec",
        summary: "Run one statement that changes something, and get the row count back.",
        detail: "INSERT, UPDATE, DELETE, CREATE, DROP, ALTER, or a PRAGMA that sets a value. A \
                 statement with RETURNING gives its rows back as well as its count.",
        params: EXEC_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::exec,
    },
    Command {
        name: "batch",
        summary: "Run several statements as one transaction.",
        detail: "Separate them with semicolons. Either all of them take effect or none of them \
                 do, which is what you want when creating a schema or loading related rows.",
        params: BATCH_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::batch,
    },
    Command {
        name: "run",
        summary: "Run shell input, dot commands and all, and get back what it printed.",
        detail: "The escape hatch, and the reason every dot command is reachable from every front \
                 end: this drives the same shell 'inillucent shell' does. Use it for the things \
                 that have no verb of their own - '.eqp on', '.parameter set', '.testcase', \
                 '.archive'. Run 'inillucent run \".help\"' for the full list of dot commands.",
        params: RUN_PARAMS,
        cli_only: None,
        // **The verb gate stands aside and the shell refuses the writes.**
        // `writes: true` here refused `--readonly run "SELECT count(*) FROM t;"`
        // and the `inillucent_run` MCP tool with it, although `run` is how a
        // read only agent reaches every dot command (task-2066 section 4.2,
        // item 26).
        writes: Writes::PerStatement,
        run: verbs::run_input,
    },
    Command {
        name: "create",
        summary: "Make a new, empty database file.",
        detail: "Refuses a path that already exists, so it can never destroy a database by being \
                 run twice. Every other command opens whatever it is pointed at.",
        params: CREATE_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::create,
    },
    Command {
        name: "tables",
        summary: "List the tables and views.",
        detail: "The names, with what each one is. System tables whose names begin with sqlite_ \
                 are left out.",
        params: PATTERN_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::tables,
    },
    Command {
        name: "describe",
        summary: "Everything about one table: columns, types, keys, indexes, row count and DDL.",
        detail: "Call this before writing SQL against a table you did not create. It answers in \
                 one call what four separate pragmas would, which matters because a caller that \
                 has to make four usually makes three and writes its query from an incomplete \
                 picture.",
        params: DESCRIBE_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::describe,
    },
    Command {
        name: "schema",
        summary: "Show the CREATE statements for the whole database or for what a pattern names.",
        detail: "The schema as SQL, which is the form you can paste into another database.",
        params: SCHEMA_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::schema,
    },
    Command {
        name: "indexes",
        summary: "List the indexes and the table each one is on.",
        detail: "Including the ones a UNIQUE constraint or a primary key created, which is why \
                 an index you did not write may appear here.",
        params: PATTERN_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::indexes,
    },
    Command {
        name: "databases",
        summary: "List the attached databases and the file behind each.",
        detail: "'main' is the one that was opened; others come from ATTACH. 'temp' is the \
                 session's own scratch database and has no file.",
        params: DB_ONLY,
        cli_only: None,
        writes: Writes::No,
        run: verbs::databases,
    },
    Command {
        name: "explain",
        summary: "Show the query plan for a statement without running it.",
        detail: "Which indexes are used, which scans are full, and in what order the tables are \
                 joined. This is how you find out why a query is slow before making it faster.",
        params: EXPLAIN_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::explain,
    },
    Command {
        name: "import",
        summary: "Load a CSV or tab-separated file into a table.",
        detail: "The table is created from the file's first row if it does not exist. Quoting is \
                 RFC 4180 unless a different format is asked for.",
        params: IMPORT_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::import,
    },
    Command {
        name: "export",
        summary: "Write a table or a query's rows out as CSV, JSON or one of six other formats.",
        detail: "With 'out' the rows go to a file and the result says so; without it they come \
                 back in the result, which is usually what an agent wants.",
        params: EXPORT_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::export,
    },
    Command {
        name: "dump",
        summary: "Render the database as the SQL that would rebuild it.",
        detail: "Schema and data, in dependency order, inside a transaction. This is the \
                 portable form: it is text, and another SQLite-speaking database will read it.",
        params: DUMP_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::dump,
    },
    Command {
        name: "backup",
        summary: "Write a copy of the database to another file.",
        detail: "A consistent copy taken while the database is open. The copy is a database, not \
                 a text dump.",
        params: BACKUP_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::backup,
    },
    Command {
        name: "restore",
        summary: "Point this session at a backup file, in place of the database it opened.",
        detail: "The opposite of 'backup', and it does not overwrite anything: this engine's \
                 databases are whole files, so restoring is opening the other file rather than \
                 writing its pages over the one you are in. That means it lasts as long as the \
                 session does - useful from the shell and from 'run', where the statements after \
                 it read the restored file, and of no effect on its own, because a one-shot \
                 process ends immediately after. To replace a file, copy the backup over it. A \
                 backup file that is not there is refused rather than created empty.",
        params: RESTORE_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::restore,
    },
    Command {
        name: "checkpoint",
        summary: "Fold the write-ahead log back into the database file.",
        detail: "Writes go to a log first and are folded in later. Doing it now shrinks the log \
                 and is what you want before copying the file by hand.",
        params: DB_ONLY,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::checkpoint,
    },
    Command {
        name: "integrity-check",
        summary: "Read every page and report whether the database holds together.",
        detail: "Answers 'ok' on a healthy database. Anything else names what is wrong. It reads \
                 the whole file, so it costs what the file costs.",
        params: DB_ONLY,
        cli_only: None,
        writes: Writes::No,
        run: verbs::integrity_check,
    },
    Command {
        name: "analyze",
        summary: "Gather the statistics the query planner reads.",
        detail: "Run it after loading a lot of data. Without statistics the planner guesses at \
                 how selective an index is, and a wrong guess is the usual reason a query that \
                 should use an index does not.",
        params: ANALYZE_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::analyze,
    },
    Command {
        name: "stats",
        summary: "Report the page cache, the pool size and the shape of the file.",
        detail: "Cache hits and misses, how many pages the file holds and how many are free. \
                 This is where you look when a workload is slower than it should be.",
        params: DB_ONLY,
        cli_only: None,
        writes: Writes::No,
        run: verbs::stats,
    },
    Command {
        name: "search",
        summary: "Full-text search over an FTS5 or inillucent_search table.",
        detail: "Writes the MATCH ... ORDER BY rank idiom for you, which is the part nobody \
                 remembers. The table has to be a full-text one; 'describe' will show you \
                 whether it is.",
        params: SEARCH_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::search,
    },
    Command {
        name: "vector-search",
        summary: "Find the rows whose vector is nearest to one you supply.",
        detail: "Over a VECTOR(N) column, by cosine distance unless you ask for another measure. \
                 If there is an HNSW index on the column the planner uses it; if there is not, \
                 this is an exhaustive scan and is still correct.",
        params: VECTOR_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::vector_search,
    },
    Command {
        name: "capabilities",
        summary: "Ask what this engine can do before composing a statement.",
        detail: "Every row is checked against the running engine by a test, in both directions - \
                 a claim of support that fails and a claim of absence that now works each turn \
                 the build red. So this is worth trusting in a way a hand-maintained feature \
                 list is not. A name that is not in the table answers no, because a capability \
                 that was never declared was never checked.",
        params: CAPABILITY_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::capabilities,
    },
    Command {
        name: "functions",
        summary: "List the SQL functions this engine answers.",
        detail: "From the engine's own register, which is compared against the reference \
                 library's on every build - so this is what actually exists rather than what was \
                 documented once.",
        params: PATTERN_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::functions,
    },
    Command {
        name: "migrate",
        summary: "Build an inillucent database from a SQLite file, PostgreSQL or MySQL.",
        detail: "Reads the source and writes a new .rdb with the same rows. A path is a SQLite \
                 database file; a postgres:// or mysql:// URL is a running server, read inside \
                 one repeatable-read snapshot so that every table is as of one instant. The \
                 source is never written to and the destination is never overwritten: the new \
                 file is staged under another name and published by a rename, so a half-written \
                 database never sits where an application would open it. A server migration is \
                 verified per table by row count and by an order-independent digest, and nothing \
                 that fails a check is published.",
        params: MIGRATE_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: verbs::migrate,
    },
    Command {
        name: "setup-embeddings",
        summary: "Download and install the embedding model and the runtime it needs.",
        detail: "One command, on Windows, macOS and Linux. It fetches ONNX Runtime and the \
                 nomic-embed-text-v1.5 weights into a per-user directory, checks every byte \
                 against a digest pinned in this build, and leaves the engine able to answer \
                 embed(TEXT) with nothing exported by hand - so a mismatch is a refusal that \
                 names both digests rather than a shared library that loads and misbehaves. \
                 Name what you want: 'all' installs both halves, 'runtime' and 'model' one \
                 each. Run with no component at all and it reports what is installed and \
                 downloads nothing, which is what stops a 620 MB fetch being a surprise; \
                 '--status' does the same explicitly. About 620 MB the first time and \
                 nothing on a later run. '--residency' chooses when the model is in memory: \
                 'resident' keeps it, which is about 1.9 GB held and 12 to 36 ms a query; \
                 'on-demand' loads it per call, which holds nothing and costs about 0.8 s a \
                 query; 'idle' or 'idle:90s' loads it on use and drops it after a quiet \
                 period, which is the default and pays the load once for a burst of \
                 questions.",
        params: SETUP_PARAMS,
        cli_only: None,
        writes: Writes::Yes,
        run: crate::setup::setup_embeddings,
    },
    Command {
        name: "version",
        summary: "Report the engine, the dialect and the driver versions.",
        detail: "The SQLite version named here is the dialect this engine implements, not a \
                 library it links. There is no SQLite in this binary.",
        params: VERSION_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::version,
    },
    Command {
        name: "help",
        summary: "List every command, or explain one in full.",
        detail: "With no topic it prints the table. With one it prints that command's usage, \
                 what it is for, and every parameter it takes.",
        params: HELP_PARAMS,
        cli_only: None,
        writes: Writes::No,
        run: verbs::help,
    },
    Command {
        name: "shell",
        summary: "Start the interactive shell.",
        detail: "The sqlite3-shaped REPL, with all 63 dot commands. Everything it can do is also \
                 reachable non-interactively through 'run'.",
        params: &[],
        cli_only: Some(
            "it is a terminal REPL: it reads a keyboard and writes a screen, and neither exists \
             at the other end of an MCP call. Use 'run' instead, which drives the same shell.",
        ),
        writes: Writes::Yes,
        run: verbs::shell_placeholder,
    },
    Command {
        name: "mcp",
        summary: "Serve these commands to an agent over MCP on standard input and output.",
        detail: "Every command in this table that is not marked cli-only becomes a tool named \
                 inillucent_<command>, with this same description and these same parameters.",
        params: &[],
        cli_only: Some(
            "it is the server that would be exposing the tools, so offering it as one of them \
             would let a client ask the server to serve itself.",
        ),
        writes: Writes::Yes,
        run: verbs::mcp_placeholder,
    },
];
