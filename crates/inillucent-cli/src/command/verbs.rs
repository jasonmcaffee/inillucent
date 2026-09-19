//! What each command in the table actually does.
//!
//! Invariant: **every one of these drives the shell.** Some run SQL through
//! `Shell::collect`, some run a dot command through `Shell` and collect what it
//! printed, and none of them touches the engine directly. That is the same
//! constraint the shell itself carries, one level up, and it is what makes the
//! 416-case differential probe cover this surface too.
//!
//! Where a verb is a thin wrapper over a dot command, it is a *deliberate* thin
//! wrapper: `.dump` and `.import` are the reference's own, they have been
//! compared against `sqlite3` byte for byte, and a second implementation here
//! would be a second set of quoting rules. Where a verb produces a table
//! instead - `tables`, `describe`, `capabilities` - it is because a caller that
//! is a program wants rows rather than a paragraph.

use inillucent_driver::{Status, Support};
use inillucent_value::Value;

use super::outcome::{columns_from, table, Column, Failed, Outcome};
use super::{Arguments, Context};
use crate::json::{self, Json};

/// Turns one engine value into the JSON a result carries.
///
/// A blob becomes its hexadecimal spelling with an `x''` wrapper, because that
/// is a form the engine will read back as the same bytes, and because JSON has
/// no byte string. It is text in the document and says so in the column type.
///
/// @param value - the cell the engine produced
pub fn value_to_json(value: &Value<'static>) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Integer(number) => Json::Int(*number),
        Value::Real(number) => Json::Real(*number),
        Value::Text(text) => json::text(String::from_utf8_lossy(text.raw()).into_owned()),
        Value::Blob(bytes) => {
            let mut rendered = String::from("x'");
            for byte in bytes.raw() {
                rendered.push_str(&format!("{byte:02x}"));
            }
            rendered.push('\'');
            json::text(rendered)
        }
    }
}

/// Turns one JSON value into the SQL literal that reproduces it.
///
/// **Scalars only.** An array or an object is not a SQL value, and accepting one
/// by rendering it as its JSON text would bind a string where the caller meant a
/// structure - which is wrong quietly, in the database, rather than loudly, here.
///
/// @param value - what the caller passed
fn literal_of(value: &Json) -> Result<String, Failed> {
    match value {
        Json::Null => Ok("NULL".to_string()),
        Json::Bool(true) => Ok("1".to_string()),
        Json::Bool(false) => Ok("0".to_string()),
        Json::Int(number) => Ok(number.to_string()),
        Json::Real(number) if number.is_finite() => Ok(format!("{number:?}")),
        Json::Real(_) => Err(Failed::misuse(
            "a parameter cannot be NaN or infinity: SQL has no spelling for either.",
        )),
        // An `x'..'` string round-trips as the blob it names, which is how a
        // blob leaves in `value_to_json` and so how it must be allowed back in.
        Json::Text(text) if is_blob_literal(text) => Ok(text.clone()),
        Json::Text(text) => Ok(format!("'{}'", text.replace('\'', "''"))),
        // **An array of numbers is a vector (task-1979, section 8.2, gap 2).**
        // It was refused, and the only working spelling of a vector parameter
        // was a hex blob the caller had to assemble itself - so `--params` was
        // unusable for the first write into a `VECTOR(N)` column, which is the
        // first thing an application does with one.
        Json::Array(values) => vector_literal(values),
        // **A blob is `{"blob": "<hex>"}`.** JSON has no byte string, and the
        // `x'..'` text form above only covers a value this command printed; a
        // caller with bytes of its own had no spelling at all.
        Json::Object(fields) => blob_literal(fields),
    }
}

/// Returns the blob literal a JSON array of numbers names.
///
/// The bytes are little-endian 32-bit floats, which is what a `VECTOR(N)`
/// column holds and what `vector_distance_cos` reads.
///
/// @param values - the array's elements
fn vector_literal(values: &[Json]) -> Result<String, Failed> {
    if values.is_empty() {
        return Err(Failed::misuse(
            "a parameter that is an array is a vector, so it needs at least one number.",
        ));
    }
    let mut hex = String::from("x'");
    for value in values {
        let number = match value {
            Json::Int(whole) => *whole as f64,
            Json::Real(real) if real.is_finite() => *real,
            _ => {
                return Err(Failed::misuse(
                    "a parameter that is an array is a vector, so every element has to be a \
                     finite number.",
                ))
            }
        };
        for byte in (number as f32).to_le_bytes() {
            hex.push_str(&format!("{byte:02x}"));
        }
    }
    hex.push('\'');
    Ok(hex)
}

/// Returns the blob literal a `{"blob": "<hex>"}` parameter names.
///
/// @param fields - the object's members
fn blob_literal(fields: &[(String, Json)]) -> Result<String, Failed> {
    let held = fields
        .iter()
        .find(|(name, _)| name == "blob")
        .map(|(_, value)| value);
    let Some(Json::Text(hex)) = held else {
        return Err(Failed::misuse(
            "a parameter has to be a string, a number, a boolean, null, an array of numbers \
             for a vector, or {\"blob\": \"<hex>\"} for bytes.",
        ));
    };
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return Err(Failed::misuse(
            "the value of \"blob\" has to be an even number of hexadecimal digits.",
        ));
    }
    Ok(format!("x'{hex}'"))
}

/// Returns whether a string is the `x'..'` spelling of a blob.
///
/// @param text - the candidate
fn is_blob_literal(text: &str) -> bool {
    let Some(inner) = text
        .strip_prefix("x'")
        .and_then(|rest| rest.strip_suffix('\''))
    else {
        return false;
    };
    !inner.is_empty()
        && inner.len() % 2 == 0
        && inner.chars().all(|digit| digit.is_ascii_hexdigit())
}

/// Quotes an identifier the way the engine reads it back.
///
/// @param name - the object name
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quotes a string as a SQL text literal.
///
/// @param text - the value
fn quoted_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Binds the caller's parameters, runs the statement, and builds the outcome.
///
/// Parameters are bound **by position** - `?1`, `?2`, ... in the order the
/// array gives them - through `Shell::collect_bound`. A value becomes an engine
/// value by being selected: `SELECT <literal>` puts the engine's own literal
/// reader in the path rather than a second one here, which is the same thing
/// `.parameter set` does and for the same reason.
///
/// @param context - where to run
/// @param command - the verb, for the outcome
/// @param sql - the statement
/// @param params - the values for `?1`, `?2`, ...
/// @param limit - how many rows to hand back
fn produce(
    context: &mut Context,
    command: &str,
    sql: &str,
    params: &[Json],
    limit: usize,
) -> Result<Outcome, Failed> {
    context.refuse_if_it_writes(sql)?;
    refuse_a_script(context, command, sql)?;
    let mut bound = Vec::with_capacity(params.len());
    for value in params {
        let literal = literal_of(value)?;
        let held = context
            .shell()
            .collect(&format!("SELECT {literal}"))
            .map_err(|failure| Failed::from_shell(&failure))?
            .1
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(Value::Null);
        bound.push(inillucent_tree::datum::OwnedDatum::from(&held));
    }
    let started = std::time::Instant::now();
    let collected = context.shell().collect_bound(sql, &bound);
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let (names, rows) = collected.map_err(|failure| Failed::from_shell(&failure))?;
    Ok(rows_to_outcome(
        context, command, names, rows, limit, elapsed,
    ))
}

/// Turns collected rows into an outcome, cut to the caller's limit.
///
/// @param context - for the null placeholder and the change counters
/// @param command - the verb
/// @param names - the column names
/// @param rows - every row the statement produced
/// @param limit - how many to hand back, zero meaning all of them
/// @param elapsed - how long the statement took, in milliseconds
fn rows_to_outcome(
    context: &mut Context,
    command: &str,
    names: Vec<String>,
    rows: Vec<Vec<Value<'static>>>,
    limit: usize,
    elapsed: f64,
) -> Outcome {
    let total = rows.len();
    let kept = if limit == 0 { total } else { limit.min(total) };
    let cells: Vec<Vec<Json>> = rows
        .iter()
        .take(kept)
        .map(|row| row.iter().map(value_to_json).collect())
        .collect();
    let columns = columns_from(&names, &cells);
    let connection = context.shell().connection();
    let changes = connection.total_changes().unwrap_or_default();
    let rowid = connection.last_insert_rowid().unwrap_or_default();
    let _ = connection;
    let mut text = table(&columns, &cells, &context.null);
    if kept < total {
        text.push_str(&format!("\n({kept} of {total} rows)"));
    }
    Outcome {
        command: command.to_string(),
        columns,
        rows: cells,
        total,
        more: kept < total,
        changes,
        last_insert_rowid: rowid,
        elapsed_ms: elapsed,
        text,
        extra: Vec::new(),
    }
}

/// Returns the limit a call asked for, or the context's default.
///
/// Zero means every row, which is what an export wants and what a console
/// never does.
///
/// @param context - for the default
/// @param arguments - what was passed
fn limit_of(context: &Context, arguments: &Arguments) -> Result<usize, Failed> {
    let asked = match arguments.integer("limit") {
        // **A negative limit used to become zero, and zero means every row.**
        // So `limit=-1` - which is how a caller spells "no limit" in most other
        // things, and how an off-by-one in a client's arithmetic comes out -
        // asked a confined server for the whole table. It is a refusal now.
        Some(asked) if asked < 0 => {
            return Err(Failed::misuse(format!(
                "limit={asked} is not a number of rows. Write 0 for every row, or a positive \
                 count."
            )))
        }
        Some(asked) => asked as usize,
        None => context.limit,
    };
    context.cap_rows(asked)
}

/// Refuses a script where one statement was asked for.
///
/// `query` and `exec` both compile one statement and run it. Handed several, they used to compile
/// the first, run it, and report success - so `inillucent exec "<twenty CREATE TABLEs>"` produced a
/// database holding one table and printed `ok. 0 rows changed.` Nothing said the other nineteen had
/// not run. `batch` is the verb for several statements, and it runs them in one transaction, so the
/// refusal can name it.
///
/// @param context - the open shell
/// @param command - which verb is refusing, so the message names it
/// @param sql - the text the caller passed
fn refuse_a_script(context: &mut Context, command: &str, sql: &str) -> Result<(), Failed> {
    let Some(rest) = context.shell().trailing_statement(sql) else {
        return Ok(());
    };
    Err(Failed::said(
        Status::InvalidState,
        format!(
            "{command} runs one statement and this is several; the next one begins {rest:?}. \
             Use `batch`, which runs them all in one transaction."
        ),
    ))
}

/// Returns the values to bind, from `params` or from `params-file`.
///
/// **A command line has a length ceiling and a parameter can be past it
/// (task-1979, D17).** About 32 KB on Windows: the Node and PHP wrappers spawn
/// this binary and put the JSON array in an argument, so a parameter larger
/// than that failed outright with an operating system error rather than with
/// anything about SQL. A file, or `-` for standard input, has no such limit,
/// and it is also where `{"blob": "<hex>"}` becomes practical - bytes are
/// exactly what a caller has a lot of.
///
/// Naming both is a refusal rather than a precedence rule, because a caller
/// that supplied two sets of values has made a mistake and guessing which one
/// it meant is how the wrong values get bound.
///
/// @param arguments - the command line as it was parsed
fn bound_values(arguments: &Arguments) -> Result<Vec<Json>, Failed> {
    let inline = arguments.values("params");
    let Some(named) = arguments.text("params-file") else {
        return Ok(inline);
    };
    if !inline.is_empty() {
        return Err(Failed::misuse(
            "give the values in 'params' or in 'params-file', not both.",
        ));
    }
    let text = match named {
        "-" => {
            let mut held = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut held)
                .map_err(|error| Failed::said(Status::Io, format!("standard input: {error}")))?;
            held
        }
        path => std::fs::read_to_string(path)
            .map_err(|error| Failed::said(Status::Io, format!("{path}: {error}")))?,
    };
    let parsed = json::parse(text.trim())
        .map_err(|why| Failed::misuse(format!("'params-file' is not JSON: {why}")))?;
    match parsed {
        Json::Array(values) => Ok(values),
        _ => Err(Failed::misuse("'params-file' has to hold a JSON array.")),
    }
}

/// `query`: runs a statement that returns rows.
pub fn query(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let sql = arguments.required_text("sql")?.to_string();
    let params = bound_values(arguments)?;
    let limit = limit_of(context, arguments)?;
    produce(context, "query", &sql, &params, limit)
}

/// `exec`: runs one statement for its effect.
pub fn exec(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let sql = arguments.required_text("sql")?.to_string();
    let params = bound_values(arguments)?;
    let before = context
        .shell()
        .connection()
        .total_changes()
        .map_err(|error| Failed::from_engine(&error))?;
    let mut produced = produce(context, "exec", &sql, &params, 0)?;
    let after = context
        .shell()
        .connection()
        .total_changes()
        .map_err(|error| Failed::from_engine(&error))?;
    produced.changes = after - before;
    produced.text = match produced.rows.is_empty() {
        true => format!(
            "ok. {} row{} changed.",
            produced.changes,
            if produced.changes == 1 { "" } else { "s" }
        ),
        // `RETURNING` makes a write produce rows, and a caller that asked for
        // them should be shown them rather than a count they did not ask about.
        false => produced.text.clone(),
    };
    Ok(produced)
}

/// `batch`: runs several statements as one transaction.
///
/// **It is a transaction as of task-1932, and until then it was not.** The
/// command's own description says "either all of them take effect or none of
/// them do, which is what you want when creating a schema or loading related
/// rows", and the MCP tool `inillucent_batch` inherits that description - but
/// nothing opened a transaction. `execute_batch` is a loop of `execute_any`
/// with nothing around it, so each statement committed as it succeeded, and
/// `inillucent batch "INSERT ...; INSERT ...; GARBAGE"` reported failure with
/// two rows committed. That is the exact case the description names as the
/// reason to use it.
///
/// A script run inside a transaction the caller already opened joins it and
/// does not commit: closing somebody else's transaction because a command
/// inside it finished would be a worse surprise than the one being fixed, and
/// the outcome's `detail` says which of the two happened. An explicit `BEGIN`
/// inside the script is left to the engine, which refuses it with "cannot
/// start a transaction within a transaction".
pub fn batch(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let sql = arguments.required_text("sql")?.to_string();
    context.refuse_if_it_writes(&sql)?;
    let joined = !context
        .shell()
        .connection()
        .autocommit()
        .map_err(|error| Failed::from_engine(&error))?;
    let before = context
        .shell()
        .connection()
        .total_changes()
        .map_err(|error| Failed::from_engine(&error))?;
    if !joined {
        context
            .shell()
            .execute("BEGIN")
            .map_err(|message| Failed::said(Status::Syntax, message))?;
    }
    let ran = context.shell().execute(&sql);
    if let Err(message) = ran {
        if !joined {
            // **The rollback's own failure is not reported over the
            // statement's.** The script's error is what the caller asked
            // about; a rollback that could not run is reported beside it
            // rather than instead of it, because a caller who reads only
            // "cannot rollback" learns nothing about what went wrong.
            if let Err(second) = context.shell().execute("ROLLBACK") {
                return Err(Failed::said(
                    Status::Syntax,
                    format!("{message} (and the rollback failed: {second})"),
                ));
            }
        }
        return Err(Failed::said(Status::Syntax, message));
    }
    if !joined {
        context
            .shell()
            .execute("COMMIT")
            .map_err(|message| Failed::said(Status::Syntax, message))?;
    }
    let after = context
        .shell()
        .connection()
        .total_changes()
        .map_err(|error| Failed::from_engine(&error))?;
    let changes = after - before;
    let mut produced = Outcome::said(
        "batch",
        format!(
            "ok. {changes} row{} changed.",
            if changes == 1 { "" } else { "s" }
        ),
    );
    produced.changes = changes;
    produced.extra.push((
        "transaction".to_string(),
        Json::Text(
            if joined {
                "joined the open transaction; not committed"
            } else {
                "committed"
            }
            .to_string(),
        ),
    ));
    Ok(produced)
}

/// `run`: runs shell input, dot commands included, and returns what it printed.
pub fn run_input(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let input = arguments.required_text("input")?.to_string();
    if context.readonly() {
        for line in input.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('.') {
                continue;
            }
            context.refuse_if_it_writes(trimmed)?;
        }
    }
    let printed = context.collect_output(&input);
    let failed = context.shell().failed;
    context.shell().failed = false;
    let mut produced = Outcome::said("run", printed.trim_end());
    produced = produced.with("shell_reported_an_error", Json::Bool(failed));
    Ok(produced)
}

/// `create`: makes a new database file.
pub fn create(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let path = arguments.required_text("path")?.to_string();
    let confined = context.confine(&path)?;
    if confined.exists() {
        return Err(Failed::said(
            Status::InvalidState,
            format!("\"{path}\" already exists. Open it instead of creating it."),
        ));
    }
    let named = confined.to_string_lossy().into_owned();
    context.use_database(&named)?;
    // A database file with no objects in it is not written until something is,
    // so the file a caller asked for has to be brought into existence by an
    // actual write. `user_version` is the smallest one that changes no schema.
    context
        .shell()
        .execute("PRAGMA user_version = 0")
        .map_err(|message| Failed::said(Status::Io, message))?;
    Ok(Outcome::said("create", format!("created {named}")).with("path", json::text(&named)))
}

/// Runs a statement and returns its rows as an outcome, without binding.
///
/// @param context - where to run
/// @param command - the verb
/// @param sql - the statement
fn listing(context: &mut Context, command: &str, sql: &str) -> Result<Outcome, Failed> {
    produce(context, command, sql, &[], 0)
}

/// `tables`: the tables and views in the database.
pub fn tables(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let mut sql = String::from(
        "SELECT name, type FROM sqlite_master WHERE type IN ('table','view') \
         AND name NOT LIKE 'sqlite_%'",
    );
    if let Some(pattern) = arguments.text("pattern") {
        sql.push_str(&format!(" AND name LIKE {}", quoted_text(pattern)));
    }
    sql.push_str(" ORDER BY name");
    listing(context, "tables", &sql)
}

/// `indexes`: the indexes in the database, and what each is on.
///
/// **Two sources, because a vector index is not written to `sqlite_master` as
/// an index (task-1979, R19).** `CREATE INDEX v ON t USING inillucent_hnsw (c)`
/// records its backing store as a virtual table, so this command listed nothing
/// at all for a database whose only index was a vector one - and the answer
/// "there are no indexes" was wrong on a file that had just been given one.
/// `PRAGMA index_list` walks the table's own index chain, which holds both
/// kinds, and reports `v` for the module owned ones.
pub fn indexes(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let pattern = arguments.text("pattern").map(str::to_string);
    let mut sql =
        String::from("SELECT name, tbl_name AS \"table\" FROM sqlite_master WHERE type = 'index'");
    if let Some(pattern) = &pattern {
        sql.push_str(&format!(" AND name LIKE {}", quoted_text(pattern)));
    }
    context.refuse_if_it_writes(&sql)?;
    let started = std::time::Instant::now();
    let (names, mut rows) = context
        .shell()
        .collect(&sql)
        .map_err(|failure| Failed::from_shell(&failure))?;
    rows.extend(module_indexes(context, pattern.as_deref())?);
    rows.sort_by(|left, right| {
        let key = |row: &Vec<Value<'static>>| {
            (
                row.get(1).map(text_of_value).unwrap_or_default(),
                row.first().map(text_of_value).unwrap_or_default(),
            )
        };
        key(left).cmp(&key(right))
    });
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    Ok(rows_to_outcome(context, "indexes", names, rows, 0, elapsed))
}

/// Returns one row per vector index, in the shape the `indexes` listing uses.
///
/// Every table is asked for its own index chain, because that chain is the one
/// place both kinds of index are recorded; see [`indexes`] for why
/// `sqlite_master` is not enough.
///
/// @param context - the open database
/// @param pattern - the `LIKE` pattern the caller gave, if any
fn module_indexes(
    context: &mut Context,
    pattern: Option<&str>,
) -> Result<Vec<Vec<Value<'static>>>, Failed> {
    let tables = context
        .shell()
        .column("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'");
    let mut found = Vec::new();
    for table in tables {
        let listed = context
            .shell()
            .collect(&format!("PRAGMA index_list({})", quoted_text(&table)))
            .map_err(|failure| Failed::from_shell(&failure))?
            .1;
        for row in listed {
            if row.get(3).map(text_of_value).as_deref() != Some("v") {
                continue;
            }
            let Some(name) = row.get(1).map(text_of_value) else {
                continue;
            };
            if let Some(pattern) = pattern {
                if !like(&name, pattern) {
                    continue;
                }
            }
            let (Ok(named), Ok(owner)) = (
                Value::owned_text(name.as_bytes()),
                Value::owned_text(table.as_bytes()),
            ) else {
                continue;
            };
            found.push(vec![named, owner]);
        }
    }
    Ok(found)
}

/// Returns a value's text, or the empty string for anything else.
///
/// @param value - the cell
fn text_of_value(value: &Value<'static>) -> String {
    match value {
        Value::Text(text) => String::from_utf8_lossy(text.raw()).into_owned(),
        _ => String::new(),
    }
}

/// Answers SQLite's `LIKE` for the two patterns this command accepts.
///
/// Only `%` is honoured, which is every pattern the command's own help
/// describes; `_` is left alone because a name holding one is commoner here
/// than a caller meaning it as a wildcard.
///
/// @param name - the index's name
/// @param pattern - what the caller asked for
fn like(name: &str, pattern: &str) -> bool {
    let folded = name.to_lowercase();
    let wanted = pattern.to_lowercase();
    let parts: Vec<&str> = wanted.split('%').collect();
    let mut at = 0usize;
    for (which, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(found) = folded.get(at..).and_then(|rest| rest.find(part)) else {
            return false;
        };
        if which == 0 && !wanted.starts_with('%') && found != 0 {
            return false;
        }
        at = at.saturating_add(found).saturating_add(part.len());
    }
    if !wanted.ends_with('%') {
        if let Some(last) = parts.last() {
            if !last.is_empty() && at != folded.len() {
                return false;
            }
        }
    }
    true
}

/// `databases`: what is attached, and the file behind each.
pub fn databases(context: &mut Context, _arguments: &Arguments) -> Result<Outcome, Failed> {
    listing(context, "databases", "PRAGMA database_list")
}

/// `schema`: the `CREATE` statements, as the shell writes them.
pub fn schema(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let mut line = String::from(".schema");
    if arguments.flag("indent") {
        line.push_str(" --indent");
    }
    if let Some(pattern) = arguments.text("pattern") {
        line.push(' ');
        line.push_str(pattern);
    }
    let printed = context.collect_output(&line);
    context.shell().failed = false;
    Ok(Outcome::said("schema", printed.trim_end()))
}

/// `describe`: everything about one table, in one call.
///
/// **One call on purpose.** A model that has to make four - `table_info`,
/// `index_list`, `foreign_key_list`, then the DDL - makes three of them and
/// answers from an incomplete picture. This is the single most useful tool on
/// the list for an agent, and it is the one whose absence was most visible when
/// the local model was first pointed at the server.
pub fn describe(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let name = arguments.required_text("table")?.to_string();
    let info = format!("PRAGMA table_info({})", quoted(&name));
    let mut produced = produce(context, "describe", &info, &[], 0)?;
    if produced.rows.is_empty() {
        return Err(Failed::said(
            Status::NotFound,
            format!("no such table: {name}"),
        ));
    }
    let ddl = context
        .shell()
        .scalar(&format!(
            "SELECT sql FROM sqlite_master WHERE name = {}",
            quoted_text(&name)
        ))
        .unwrap_or_default();
    let index_rows = context.shell().column(&format!(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = {} ORDER BY name",
        quoted_text(&name)
    ));
    let count = context
        .shell()
        .scalar(&format!("SELECT count(*) FROM {}", quoted(&name)))
        .unwrap_or_default();
    let indexes: Vec<Json> = index_rows.iter().map(json::text).collect();
    let drawn = table(&produced.columns, &produced.rows, &context.null);
    produced.text = format!(
        "{name}: {} column{}, {count} row{}\n\n{drawn}\n\nindexes: {}\n\n{ddl}",
        produced.rows.len(),
        if produced.rows.len() == 1 { "" } else { "s" },
        if count == "1" { "" } else { "s" },
        match index_rows.is_empty() {
            true => "none".to_string(),
            false => index_rows.join(", "),
        }
    );
    Ok(produced
        .with("table", json::text(&name))
        .with("ddl", json::text(ddl))
        .with("indexes", Json::Array(indexes))
        .with(
            "row_count_in_table",
            Json::Int(count.parse::<i64>().unwrap_or(-1)),
        ))
}

/// `explain`: the query plan, drawn the way the shell draws it.
pub fn explain(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let sql = arguments.required_text("sql")?.to_string();
    let plan = context
        .shell()
        .connection()
        .explain(&sql)
        .map_err(|error| Failed::from_engine(&error))?;
    let rows: Vec<Vec<Json>> = plan.iter().map(|line| vec![json::text(line)]).collect();
    let columns = vec![Column {
        name: "plan".to_string(),
        kind: "text".to_string(),
    }];
    Ok(Outcome {
        command: "explain".to_string(),
        text: plan.join("\n"),
        total: rows.len(),
        rows,
        columns,
        more: false,
        changes: 0,
        last_insert_rowid: 0,
        elapsed_ms: 0.0,
        extra: Vec::new(),
    })
}

/// Runs a dot command and hands back what it printed.
///
/// @param context - where to run
/// @param command - the verb, for the outcome
/// @param line - the dot command, already assembled
fn dot(context: &mut Context, command: &str, line: &str) -> Result<Outcome, Failed> {
    // **Safe mode is about a dot command a caller typed, and this is not one.**
    // `import`, `dump`, `export`, `backup` and `restore` are commands of the
    // table in `registry.rs` with their own parameters, and each one confined
    // its path through `Context::confine` before building this line - so the
    // check that stops `.import` reaching outside an MCP server has already
    // been made, by the command, against the argument the caller passed. Left
    // on, it refused `inillucent_import` over MCP with
    // `.import is prohibited in safe mode`, which is a refusal of the server's
    // own verb rather than of anything the caller could have escaped through.
    let guarded = std::mem::replace(&mut context.shell().safe, false);
    let printed = context.collect_output(line);
    context.shell().safe = guarded;
    let failed = context.shell().failed;
    context.shell().failed = false;
    if failed {
        return Err(Failed::said(Status::Syntax, printed.trim_end().to_string()));
    }
    Ok(Outcome::said(command, printed.trim_end()))
}

/// `dump`: the database as the SQL that rebuilds it.
pub fn dump(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let mut line = String::from(".dump");
    if arguments.flag("data_only") {
        line.push_str(" --data-only");
    }
    if let Some(objects) = arguments.text("objects") {
        line.push(' ');
        line.push_str(objects);
    }
    dot(context, "dump", &line)
}

/// `import`: reads a delimited file into a table.
pub fn import(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let file = arguments.required_text("file")?.to_string();
    let table_name = arguments.required_text("table")?.to_string();
    let confined = context.confine(&file)?;
    let mut line = String::from(".import");
    match arguments.text("format").unwrap_or("csv") {
        "csv" => line.push_str(" --csv"),
        "ascii" => line.push_str(" --ascii"),
        "tabs" => line.push_str(" --colsep \"\t\""),
        other => {
            return Err(Failed::misuse(format!(
                "'{other}' is not a format this reads. Use csv, tabs or ascii."
            )))
        }
    }
    if let Some(skip) = arguments.integer("skip") {
        line.push_str(&format!(" --skip {skip}"));
    }
    line.push_str(&format!(
        " \"{}\" \"{table_name}\"",
        confined.to_string_lossy()
    ));
    // Counted two ways, because neither alone is right. `total_changes` is what
    // an ordinary table's insert moves and it is exact. A **virtual** table's
    // insert does not move it at all, so a 2,661 row load into an FTS5 table
    // reported `imported 0 rows` while every one of those rows was in fact
    // there - a number that says the opposite of what happened. The row count
    // of the target covers that case, and is the fallback rather than the
    // primary because a table with a trigger on it can change more rows than it
    // gained.
    let before_changes = context
        .shell()
        .connection()
        .total_changes()
        .map_err(|error| Failed::from_engine(&error))?;
    let before_rows = row_count(context, &table_name);
    let mut produced = dot(context, "import", &line)?;
    let after_changes = context
        .shell()
        .connection()
        .total_changes()
        .map_err(|error| Failed::from_engine(&error))?;
    produced.changes = after_changes - before_changes;
    if produced.changes == 0 {
        produced.changes = row_count(context, &table_name).saturating_sub(before_rows);
    }
    if produced.text.is_empty() {
        produced.text = format!("imported {} rows into {table_name}", produced.changes);
    }
    Ok(produced)
}

/// Returns how many rows a table holds, or zero when it holds none or is absent.
///
/// @param context - the open database
/// @param table - the table to count
fn row_count(context: &mut Context, table: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM \"{}\"", table.replace('"', "\"\""));
    context
        .shell()
        .scalar(&sql)
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(0)
}

/// `export`: writes rows out in a chosen format.
pub fn export(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let sql = match (arguments.text("sql"), arguments.text("table")) {
        (Some(_), Some(_)) => {
            return Err(Failed::misuse(
                "export accepts either 'sql' or 'table', not both.",
            ))
        }
        (Some(sql), None) => sql.to_string(),
        (None, Some(name)) => format!("SELECT * FROM {}", quoted(name)),
        (None, None) => return Err(Failed::misuse("export needs either 'sql' or 'table'.")),
    };
    let format = arguments.text("format").unwrap_or("csv").to_string();
    let mode = match format.as_str() {
        "csv" | "json" | "tabs" | "markdown" | "insert" | "quote" | "line" | "html" => format,
        other => {
            return Err(Failed::misuse(format!(
                "'{other}' is not an export format. Use csv, json, tabs, markdown, insert, \
                 quote, line or html."
            )))
        }
    };
    context.refuse_if_it_writes(&sql)?;
    let mut script = format!(".mode {mode}\n.headers on\n");
    if let Some(out) = arguments.text("out") {
        let confined = context.confine(out)?;
        script.push_str(&format!(".once \"{}\"\n", confined.to_string_lossy()));
    }
    script.push_str(&sql);
    script.push(';');
    let printed = context.collect_output(&script);
    let failed = context.shell().failed;
    context.shell().failed = false;
    if failed {
        return Err(Failed::said(Status::Syntax, printed.trim_end().to_string()));
    }
    let produced = Outcome::said("export", printed.trim_end());
    Ok(match arguments.text("out") {
        Some(out) => produced.with("wrote", json::text(out)),
        None => produced,
    })
}

/// `backup`: copies the database to a file.
pub fn backup(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let file = arguments.required_text("file")?.to_string();
    let confined = context.confine(&file)?;
    let named = confined.to_string_lossy().into_owned();
    context
        .shell()
        .backup_to(&named)
        .map_err(|message| Failed::said(Status::Io, message))?;
    Ok(Outcome::said("backup", format!("wrote {named}")).with("wrote", json::text(&named)))
}

/// `restore`: replaces this database's contents from a file.
pub fn restore(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let file = arguments.required_text("file")?.to_string();
    let confined = context.confine(&file)?;
    // **A backup that is not there is a refusal, not a new empty database
    // (task-1969, 5.2).** `.restore` is implemented as "open the file the
    // caller named" - see `dot.rs`, which explains why - and opening a file
    // that is not there creates it. So `inillucent --db app.rdb restore
    // typo.rdb` exited 0, said `ok`, and left the caller with an empty
    // database and no message. It is the shape task-1951 shipped in the
    // signing gate: a check that passed having checked nothing.
    if !confined.is_file() {
        return Err(Failed::said(
            Status::NotFound,
            format!(
                "{}: there is no such backup file to restore from",
                confined.to_string_lossy()
            ),
        ));
    }
    dot(
        context,
        "restore",
        &format!(".restore \"{}\"", confined.to_string_lossy()),
    )
}

/// `checkpoint`: writes the log back into the database file.
pub fn checkpoint(context: &mut Context, _arguments: &Arguments) -> Result<Outcome, Failed> {
    listing(context, "checkpoint", "PRAGMA wal_checkpoint")
}

/// `integrity-check`: reads every page and says whether it holds together.
pub fn integrity_check(context: &mut Context, _arguments: &Arguments) -> Result<Outcome, Failed> {
    listing(context, "integrity-check", "PRAGMA integrity_check")
}

/// `analyze`: gathers the statistics the planner reads.
pub fn analyze(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let sql = match arguments.text("table") {
        Some(name) => format!("ANALYZE {}", quoted(name)),
        None => "ANALYZE".to_string(),
    };
    context
        .shell()
        .execute(&sql)
        .map_err(|message| Failed::said(Status::Syntax, message))?;
    Ok(Outcome::said("analyze", "ok. sqlite_stat1 is up to date."))
}

/// `stats`: what the page cache and the file are doing.
pub fn stats(context: &mut Context, _arguments: &Arguments) -> Result<Outcome, Failed> {
    let cache = context.shell().cache_stats();
    let pool = context.shell().pool_bytes();
    let pages = context
        .shell()
        .scalar("PRAGMA page_count")
        .unwrap_or_default();
    let size = context
        .shell()
        .scalar("PRAGMA page_size")
        .unwrap_or_default();
    let free = context
        .shell()
        .scalar("PRAGMA freelist_count")
        .unwrap_or_default();
    let text = format!(
        "pool bytes:      {pool}\npage size:       {size}\npage count:      {pages}\n\
         free pages:      {free}\ncache hits:      {}\ncache misses:    {}",
        cache.hits, cache.misses
    );
    Ok(Outcome::said("stats", text)
        .with("pool_bytes", Json::Int(pool as i64))
        .with("page_size", Json::Int(size.parse::<i64>().unwrap_or(0)))
        .with("page_count", Json::Int(pages.parse::<i64>().unwrap_or(0)))
        .with("free_pages", Json::Int(free.parse::<i64>().unwrap_or(0)))
        .with("cache_hits", Json::Int(cache.hits as i64))
        .with("cache_misses", Json::Int(cache.misses as i64)))
}

/// Returns how many neighbours a search was asked for.
///
/// **`--k 0` and `--k -1` used to answer one row (task-1979, R10).** The count
/// was clamped with `.max(1)`, so a caller asking for none - which a loop over
/// a configured page size does - was given one, and a caller who had computed a
/// negative count from a mistake elsewhere was given one too. Neither is what
/// was asked for, and a row nobody asked for is worse than an error.
///
/// @param arguments - the command line as it was parsed
fn neighbours_asked_for(arguments: &Arguments) -> Result<i64, Failed> {
    let k = arguments.integer("k").unwrap_or(10);
    if k < 1 {
        return Err(Failed::misuse(
            "'k' has to be one or more: it is how many rows to return.",
        ));
    }
    Ok(k)
}

/// `search`: full-text and hybrid retrieval, without writing the idiom.
///
/// One statement over an `inillucent_search` or FTS5 table, in the form both
/// modules answer: `WHERE <table> MATCH ? ORDER BY rank`. A caller that wants
/// something else writes it with `query`; this exists because the idiom is the
/// part nobody remembers.
pub fn search(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let query_text = arguments.required_text("query")?.to_string();
    let name = arguments.required_text("table")?.to_string();
    let k = neighbours_asked_for(arguments)?;
    let sql = format!(
        "SELECT rowid, * FROM {0} WHERE {0} MATCH {1} ORDER BY rank LIMIT {k}",
        quoted(&name),
        quoted_text(&query_text)
    );
    let mut produced = produce(context, "search", &sql, &[], 0)?;
    produced.command = "search".to_string();
    Ok(produced.with("query", json::text(&query_text)))
}

/// `vector-search`: the nearest rows to a vector, by cosine distance.
pub fn vector_search(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let name = arguments.required_text("table")?.to_string();
    let column = arguments.required_text("column")?.to_string();
    let numbers = arguments.values("vector");
    if numbers.is_empty() {
        return Err(Failed::misuse(
            "'vector' has to be an array of numbers, one per dimension.",
        ));
    }
    let mut blob = String::from("x'");
    for value in &numbers {
        let Some(number) = value.integer().map(|whole| whole as f64).or(match value {
            Json::Real(real) => Some(*real),
            _ => None,
        }) else {
            return Err(Failed::misuse(
                "every element of 'vector' has to be a number.",
            ));
        };
        for byte in (number as f32).to_bits().to_le_bytes() {
            blob.push_str(&format!("{byte:02x}"));
        }
    }
    blob.push('\'');
    let k = neighbours_asked_for(arguments)?;
    let measure = arguments.text("measure").unwrap_or("cos");
    let function = match measure {
        "cos" => "vector_distance_cos",
        "l2" => "vector_distance_l2",
        "dot" => "vector_dot",
        other => {
            return Err(Failed::misuse(format!(
                "'{other}' is not a measure. Use cos, l2 or dot."
            )))
        }
    };
    let sql = format!(
        "SELECT rowid, *, {function}({1}, {blob}) AS distance FROM {0} \
         WHERE {1} IS NOT NULL ORDER BY {function}({1}, {blob}) LIMIT {k}",
        quoted(&name),
        quoted(&column)
    );
    produce(context, "vector-search", &sql, &[], 0)
}

/// `capabilities`: what the engine says it does, checked in both directions.
pub fn capabilities(_context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let wanted = arguments.text("name");
    let rows: Vec<Vec<Json>> = inillucent_driver::CAPABILITIES
        .iter()
        .filter(|entry| wanted.is_none_or(|name| entry.name == name))
        .map(|entry| {
            vec![
                json::text(entry.name),
                json::text(support_name(entry.support)),
                json::text(entry.note),
            ]
        })
        .collect();
    if rows.is_empty() {
        return Err(Failed::said(
            Status::NotFound,
            format!(
                "no capability named \"{}\". An unknown name means no, never yes: a capability \
                 that was never declared was never checked.",
                wanted.unwrap_or_default()
            ),
        ));
    }
    let names = vec![
        "capability".to_string(),
        "support".to_string(),
        "note".to_string(),
    ];
    let columns = columns_from(&names, &rows);
    // **Not the aligned table, for this one command.** A note runs to two
    // hundred characters - `cancel`'s explains why a Stop button would be a lie
    // - and padding a column to the widest of those produces lines nothing can
    // read, on a terminal or in a model's context. The rows are still in the
    // result for a program; this is what a reader gets.
    let text = wrapped_notes(&rows);
    Ok(Outcome {
        command: "capabilities".to_string(),
        total: rows.len(),
        rows,
        columns,
        more: false,
        changes: 0,
        last_insert_rowid: 0,
        elapsed_ms: 0.0,
        text,
        extra: Vec::new(),
    })
}

/// Lays capability rows out as a name, a verdict and a wrapped note.
///
/// @param rows - the capability rows, name then support then note
fn wrapped_notes(rows: &[Vec<Json>]) -> String {
    let mut lines = Vec::with_capacity(rows.len() * 3);
    for row in rows {
        let name = row.first().and_then(Json::text).unwrap_or_default();
        let support = row.get(1).and_then(Json::text).unwrap_or_default();
        let note = row.get(2).and_then(Json::text).unwrap_or_default();
        lines.push(format!("{name:<22} {support}"));
        for line in wrap(note, 74) {
            lines.push(format!("    {line}"));
        }
    }
    lines.join(
        "
",
    )
}

/// Breaks a sentence into lines no wider than a limit, on word boundaries.
///
/// A word longer than the limit is left whole rather than cut: a broken
/// identifier is harder to read than a long line, and these notes name SQL
/// constructs.
///
/// @param text - the sentence
/// @param width - the widest line to produce
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Returns the word a support level is reported as.
///
/// @param support - the level
fn support_name(support: Support) -> &'static str {
    match support {
        Support::Yes => "yes",
        Support::Partial => "partial",
        Support::No => "no",
    }
}

/// `functions`: the SQL functions this engine answers.
pub fn functions(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let mut sql = String::from("PRAGMA function_list");
    let produced = listing(context, "functions", &sql);
    // `function_list` is the enumeration `registers.rs` compares against the
    // pinned library on every build, so it is the authority here. If this
    // engine ever stops answering it, saying so is better than a hand-written
    // list that would then be the only one.
    let mut produced = produced?;
    if let Some(pattern) = arguments.text("pattern") {
        let lower = pattern.to_ascii_lowercase();
        produced.rows.retain(|row| {
            row.first()
                .and_then(Json::text)
                .is_some_and(|name| name.to_ascii_lowercase().contains(&lower))
        });
        produced.total = produced.rows.len();
        produced.text = table(&produced.columns, &produced.rows, &context.null);
    }
    sql.clear();
    Ok(produced)
}

/// `migrate`: brings a SQLite file, a running server, or a legacy index into
/// this engine.
///
/// **The kind is defaulted from the source and not guessed at.** The other
/// migration tool argues, correctly, that deciding which migration to run by
/// looking at the source would "pick wrongly exactly once, on somebody's real
/// data" - but that argument is about a *directory* against a *file*, which are
/// both just paths and cannot be told apart. `postgres://host/db` is not a path
/// on any platform this runs on, so there is nothing here to be ambiguous
/// about, and the failure mode of getting it wrong is "there is no such file"
/// rather than a migration of the wrong thing. `--kind` still overrides it.
pub fn migrate(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let destination = arguments.required_text("destination")?.to_string();
    let source = resolve_source(context, arguments.text("source"))?;
    let kind = arguments
        .text("kind")
        .map(str::to_string)
        .unwrap_or_else(|| kind_of_source(&source));
    if kind == "postgres" || kind == "mysql" {
        return migrate_remote(context, &source, &destination, arguments);
    }

    let from = context.confine(&source)?;
    let to = context.confine(&destination)?;
    if !from.exists() {
        return Err(Failed::said(
            Status::NotFound,
            format!("there is no \"{source}\" to migrate from."),
        ));
    }
    if to.exists() {
        return Err(Failed::said(
            Status::InvalidState,
            format!("\"{destination}\" already exists. This tool never overwrites."),
        ));
    }
    match kind.as_str() {
        "sqlite" => migrate_sqlite_file(&from, &to),
        "index" => Err(Failed::unsupported(
            "migrate --kind index",
            "the retrieval-index migration runs in inillucent-migrate, which links the retrieval \
             engine. Run: inillucent-migrate <source-index-dir> <destination.db>",
        )),
        other => Err(Failed::misuse(format!(
            "'{other}' is not a migration kind. Use sqlite, postgres, mysql or index."
        ))),
    }
}

/// The environment variable a source may be given in instead of an argument.
const SOURCE_URL_VARIABLE: &str = "INILLUCENT_SOURCE_URL";

/// Returns the source to migrate from, in the three ways it may be given.
///
/// **A connection URL holds a password, and an argument is in the process list
/// for the whole run** - which for a large database is hours, and which every
/// other process on the machine can read. So there are three ways to say it,
/// in this order:
///
/// 1. the argument, when it is present and is not `-`;
/// 2. `INILLUCENT_SOURCE_URL`, when it holds something;
/// 3. one line of standard input, when the argument is `-`.
///
/// Standard input is only read for an explicit `-`, and never on a confined
/// surface: `--root` is how this command table is handed to an agent over MCP,
/// and MCP speaks on standard input, so a verb that read a line from it there
/// would consume the transport rather than a URL.
///
/// The line is trimmed of its newline and nothing else, because a password may
/// legitimately end in a space.
///
/// @param context - the surface, which may be confined
/// @param argument - the source the caller passed, when it passed one
fn resolve_source(context: &Context, argument: Option<&str>) -> Result<String, Failed> {
    if let Some(source) = argument {
        if source != "-" {
            return Ok(source.to_string());
        }
    }
    if let Ok(held) = std::env::var(SOURCE_URL_VARIABLE) {
        if !held.trim().is_empty() {
            return Ok(held);
        }
    }
    if argument == Some("-") {
        if context.confined() {
            return Err(Failed::said(
                Status::InvalidState,
                "this surface is confined to a directory with --root, and '-' reads the source \
                 from standard input, which such a surface does not have to itself. Set \
                 INILLUCENT_SOURCE_URL instead.",
            ));
        }
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|error| Failed::said(Status::Io, format!("standard input: {error}")))?;
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            return Err(Failed::misuse(
                "standard input held no source. Write the file path or the connection URL on one \
                 line.",
            ));
        }
        return Ok(line);
    }
    Err(Failed::misuse(format!(
        "migrate needs a source: a database file, or a postgres:// or mysql:// URL. Pass it as \
         the first argument, set {SOURCE_URL_VARIABLE}, or pass '-' to read one line from \
         standard input."
    )))
}

/// Returns the migration kind a source names, when it names one.
///
/// @param source - what the caller passed as the source
fn kind_of_source(source: &str) -> String {
    match inillucent_remote::ConnectionUrl::parse(source) {
        Ok(url) => url.scheme.name().to_string(),
        Err(_) => "sqlite".to_string(),
    }
}

/// Migrates a running PostgreSQL or MySQL server into a new `.rdb`.
///
/// **Refused when the surface is confined.** `--root DIR` exists so that an MCP
/// server can be handed to an agent without handing it the file system, and a
/// verb that dialled an arbitrary host and port would be a hole straight
/// through that: the confinement is about reach, not about paths. So a
/// confined surface refuses a remote source by name, the same way `--readonly`
/// refuses a write.
///
/// @param context - the surface, which may be confined
/// @param source - the connection URL
/// @param destination - the file to write
/// @param arguments - the rest of the command line
fn migrate_remote(
    context: &mut Context,
    source: &str,
    destination: &str,
    arguments: &Arguments,
) -> Result<Outcome, Failed> {
    if context.confined() {
        return Err(Failed::said(
            Status::InvalidState,
            "this surface is confined to a directory with --root, and a migration from a server \
             reaches a host and a port rather than a path. Run it from an unconfined command \
             line.",
        ));
    }
    let url = inillucent_remote::ConnectionUrl::parse(source)
        .map_err(|error| Failed::misuse(error.detail().unwrap_or_else(|| error.message())))?;
    let to = context.confine(destination)?;
    if to.exists() {
        return Err(Failed::said(
            Status::InvalidState,
            format!("\"{destination}\" already exists. This tool never overwrites."),
        ));
    }
    let mut plan = inillucent_remote::Plan::new(url, &to);
    // The surface's own ceiling. `command::run` has already armed it on this
    // thread, so this is belt and braces rather than the only bound - but a
    // migration is the one verb long enough that being explicit about which
    // budget it is under is worth the line.
    plan.limits = Some(context.limits());
    if let Some(batch) = arguments.integer("batch") {
        plan.batch = (batch.max(1)) as u64;
    }
    plan.insecure_plaintext = arguments.flag("insecure-plaintext");
    // **Asked before anything is dialled.** A refusal here has cost a URL parse
    // and nothing else - no socket, no staging file, and no password on a
    // wire. The message says which of the two halves of the policy is missing.
    plan.transport().map_err(|error| {
        Failed::said(
            Status::InvalidState,
            error.detail().unwrap_or_else(|| error.message()),
        )
    })?;
    let report = inillucent_remote::migrate::migrate(&plan).map_err(|error| {
        Failed::said(
            Status::Io,
            error.detail().unwrap_or_else(|| error.message()),
        )
    })?;

    let checks: Vec<json::Json> = report
        .checks
        .iter()
        .map(|check| {
            json::object(vec![
                ("name", json::text(&check.name)),
                ("passed", json::Json::Bool(check.passed)),
                ("detail", json::text(&check.detail)),
            ])
        })
        .collect();
    let tables: Vec<json::Json> = report
        .tables
        .iter()
        .map(|table| {
            json::object(vec![
                ("source", json::text(&table.source)),
                ("destination", json::text(&table.target)),
                ("rows", json::Json::Int(table.rows as i64)),
                ("digest", json::text(&table.digest)),
            ])
        })
        .collect();
    let not_carried: Vec<json::Json> = report
        .not_carried
        .iter()
        .map(|(kind, name)| {
            json::object(vec![("kind", json::text(kind)), ("name", json::text(name))])
        })
        .collect();

    // **Every check is printed whether it passed or not.** A migration that is
    // wrong is worth describing completely: knowing that the counts are right
    // and one table's digest is not is a different problem from knowing that
    // nothing arrived.
    let mut text = format!(
        "{} -> {}\n{}, {} tables, {} rows\ntransport: {}\n",
        report.source,
        to.display(),
        report.server,
        report.tables.len(),
        report.rows(),
        report.transport
    );
    for check in &report.checks {
        text.push_str(&format!("  {}\n", check.line()));
    }
    if !report.passed() {
        text.push_str(&format!(
            "verification failed; nothing was published. The staging file is at {}",
            report.staged.display()
        ));
        return Err(Failed::said(Status::Io, text));
    }
    text.push_str(&format!("published: {}", to.display()));

    Ok(Outcome::said("migrate", text)
        .with("destination", json::text(to.to_string_lossy()))
        // How the connection was made, so the answer to "were those rows
        // encrypted in transit" is in the result rather than in whoever ran it.
        .with("transport", json::text(&report.transport))
        // The **redacted** URL: an MCP call's result is written into an agent
        // transcript, and the transcript outlives the run.
        .with("source", json::text(&report.source))
        .with("server", json::text(&report.server))
        .with("rows", json::Json::Int(report.rows() as i64))
        .with("tables", json::Json::Array(tables))
        .with("checks", json::Json::Array(checks))
        .with("notCarried", json::Json::Array(not_carried)))
}

/// Imports a SQLite database file into a new `.rdb`.
///
/// **Staged and then published, never written where an application looks.**
/// `import_into` takes the target rather than deriving it because that is the
/// property a migration needs: a half-written database must not sit at the path
/// somebody is about to open. The staging name carries the process id so two
/// migrations at once cannot collide, and the rename is the publish.
///
/// @param from - the SQLite file
/// @param to - the file to write
fn migrate_sqlite_file(from: &std::path::Path, to: &std::path::Path) -> Result<Outcome, Failed> {
    let mut staged = to.as_os_str().to_os_string();
    staged.push(format!(".staging-{}", std::process::id()));
    let staged = std::path::PathBuf::from(staged);
    // **A source the reader could not read whole is refused here (task-1979,
    // M1).** The import used to drop a table whose rows it could not read and
    // carry on, so one flipped bit in a leaf page produced a published,
    // integrity-clean database with the table gone and exit code 0. The engine
    // refuses instead, and the staging file is left where it fell rather than
    // renamed over the destination.
    let imported = match inillucent_driver::Database::import_sqlite_into(from, &staged) {
        Ok(imported) => imported,
        Err(error) => {
            // **The staging file goes with the refusal.** A migration that
            // published nothing used to leave a half-built database and its log
            // segments beside the destination, named after this process, for
            // somebody to find later and wonder about.
            remove_staged(&staged);
            return Err(Failed::from_driver(error));
        }
    };
    drop(imported);
    std::fs::rename(&staged, to).map_err(|error| {
        Failed::said(
            Status::Io,
            format!(
                "built {} but could not publish it: {error}",
                staged.display()
            ),
        )
    })?;
    Ok(Outcome::said(
        "migrate",
        format!("imported {} into {}", from.display(), to.display()),
    )
    .with("destination", json::text(to.to_string_lossy())))
}

/// Removes a staging database and every log segment beside it.
///
/// A `.rdb` is a file plus its own log segments, named after it, so removing
/// the first and leaving the rest is leaving most of the bytes.
///
/// @param staged - the staging database
fn remove_staged(staged: &std::path::Path) {
    let _ = std::fs::remove_file(staged);
    let (Some(directory), Some(stem)) = (staged.parent(), staged.file_name()) else {
        return;
    };
    let stem = stem.to_string_lossy().into_owned();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{stem}-wal.")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `version`: what this build is.
pub fn version(context: &mut Context, _arguments: &Arguments) -> Result<Outcome, Failed> {
    let printed = context.collect_output(".version");
    context.shell().failed = false;
    let text = format!(
        "{}
inillucent-cli {}
{}",
        printed.trim_end(),
        env!("CARGO_PKG_VERSION"),
        inillucent_driver::version()
    );
    Ok(Outcome::said("version", text)
        .with("cli", json::text(env!("CARGO_PKG_VERSION")))
        .with("driver", json::text(inillucent_driver::version())))
}

/// `help`: the command table, or one entry from it.
pub fn help(_context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    match arguments.text("topic") {
        None => {
            let rows: Vec<Vec<Json>> = super::COMMANDS
                .iter()
                .map(|command| vec![json::text(command.name), json::text(command.summary)])
                .collect();
            let names = vec!["command".to_string(), "what it does".to_string()];
            let columns = columns_from(&names, &rows);
            let text = table(&columns, &rows, "");
            Ok(Outcome {
                command: "help".to_string(),
                total: rows.len(),
                rows,
                columns,
                more: false,
                changes: 0,
                last_insert_rowid: 0,
                elapsed_ms: 0.0,
                text,
                extra: Vec::new(),
            })
        }
        Some(topic) => {
            let Some(command) = super::find(topic) else {
                return Err(Failed::said(
                    Status::NotFound,
                    format!("there is no '{topic}' command. Run 'inillucent help' for the list."),
                ));
            };
            let mut text = format!(
                "{}\n\n{}\n\n{}",
                command.usage(),
                command.summary,
                command.detail
            );
            if !command.params.is_empty() {
                text.push_str("\n\nParameters:");
                for param in command.params {
                    text.push_str(&format!(
                        "\n  {:<12} {}{}",
                        param.name,
                        if param.required { "(required) " } else { "" },
                        param.description
                    ));
                }
            }
            Ok(Outcome::said("help", text))
        }
    }
}

/// `shell` and `mcp` are handled by the binary, and never reach the table.
///
/// They are in [`super::COMMANDS`] so that `inillucent help` lists them and so
/// that the parity test can assert their `cli_only` reason exists. Calling one
/// through the table is a mistake in a front end rather than in a request, and
/// it says so.
///
/// @param name - which of the two was reached
fn front_end_only(name: &'static str) -> Failed {
    Failed::misuse(format!(
        "'{name}' is run by the inillucent binary itself and cannot be dispatched here."
    ))
}

/// The stand-in for `shell`.
pub fn shell_placeholder(
    _context: &mut Context,
    _arguments: &Arguments,
) -> Result<Outcome, Failed> {
    Err(front_end_only("shell"))
}

/// The stand-in for `mcp`.
pub fn mcp_placeholder(_context: &mut Context, _arguments: &Arguments) -> Result<Outcome, Failed> {
    Err(front_end_only("mcp"))
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::shell::Shell;

    /// Serializes the cases that move `INILLUCENT_SOURCE_URL`.
    ///
    /// An environment variable is process-wide and `cargo test` runs the cases in
    /// one binary on several threads, so two of these racing is not a
    /// possibility - it is what happens. Three of the five below set the
    /// variable and three clear it, and a run of the five on their own failed
    /// four times out of five: a case that had just set the variable read the
    /// empty value another case had cleared, and the failure named the refusal
    /// rather than the race.
    ///
    /// The lock is poison-tolerant on purpose. A case that panics with it held
    /// has already failed and reported why; turning that into a second failure
    /// in every later case would bury the message that matters.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns a surface, confined or not.
    ///
    /// @param root - the directory to confine to, when the case wants one
    fn context(root: Option<std::path::PathBuf>) -> Context {
        Context::for_test(
            Shell::open(":memory:").expect("a memory database opens"),
            root,
        )
    }

    /// The argument is used when there is one, and it is not `-`.
    #[test]
    fn an_argument_is_the_source() {
        let context = context(None);
        let held = resolve_source(&context, Some("postgres://user@host/db"))
            .expect("the argument is accepted");
        assert_eq!(held, "postgres://user@host/db");
    }

    /// With no argument, the source comes from the environment.
    ///
    /// **Which is what this exists for.** A connection URL holds a password
    /// and an argument is in the process list for the whole run.
    #[test]
    fn the_environment_supplies_a_source_that_was_not_an_argument() {
        let _held = env_guard();
        let context = context(None);
        std::env::set_var(SOURCE_URL_VARIABLE, "postgres://user:secret@host/db");
        let held = resolve_source(&context, None).expect("the variable is read");
        std::env::remove_var(SOURCE_URL_VARIABLE);
        assert_eq!(held, "postgres://user:secret@host/db");
    }

    /// With nothing anywhere, the refusal names all three ways to say it.
    #[test]
    fn no_source_anywhere_is_refused_by_name() {
        let _held = env_guard();
        let context = context(None);
        std::env::remove_var(SOURCE_URL_VARIABLE);
        let error = resolve_source(&context, None).expect_err("there is no source");
        let said = format!("{error:?}");
        assert!(said.contains(SOURCE_URL_VARIABLE), "{said}");
        assert!(said.contains("standard input"), "{said}");
    }

    /// A confined surface refuses `-` rather than reading its own transport.
    ///
    /// `--root` is how this command table is handed to an agent over MCP, and
    /// MCP speaks on standard input: a verb that read a line from it there
    /// would consume the transport rather than a URL.
    #[test]
    fn a_confined_surface_refuses_to_read_standard_input() {
        let _held = env_guard();
        let context = context(Some(std::env::temp_dir()));
        std::env::remove_var(SOURCE_URL_VARIABLE);
        let error = resolve_source(&context, Some("-")).expect_err("a confined surface refuses");
        let said = format!("{error:?}");
        assert!(said.contains("--root"), "{said}");
        assert!(said.contains(SOURCE_URL_VARIABLE), "{said}");
    }

    /// Export refuses an ambiguous request before reading either data source.
    #[test]
    fn export_refuses_table_and_sql_together() {
        let mut arguments = Arguments::default();
        arguments.set("table", crate::json::text("expected"));
        arguments.set("sql", crate::json::text("SELECT 'other' AS v"));
        let failure = export(&mut context(None), &arguments)
            .expect_err("export must require one data source");
        assert!(failure.message.contains("not both"), "{}", failure.message);
    }

    /// `-` with the variable set takes the variable and never touches stdin.
    ///
    /// The order matters: a caller that scripts `-` and also exports the
    /// variable should not block on a transport nobody is writing to.
    #[test]
    fn the_environment_wins_over_reading_standard_input() {
        let _held = env_guard();
        let context = context(None);
        std::env::set_var(SOURCE_URL_VARIABLE, "mysql://user@host/db");
        let held = resolve_source(&context, Some("-")).expect("the variable is read");
        std::env::remove_var(SOURCE_URL_VARIABLE);
        assert_eq!(held, "mysql://user@host/db");
    }
}
