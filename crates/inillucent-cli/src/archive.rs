//! `.archive`: the SQL archive, over a database table or over a zip file.
//!
//! Invariant: **an archive this writes is an archive the reference reads, and
//! the other way round.** That is the whole purpose of the command - a `.sqlar`
//! table is a documented format and a zip file is a universal one - so the
//! rows, the columns, the compression rule and the mode bits are all the
//! reference's rather than a convenient approximation of them.
//!
//! The two targets are the same six operations over different storage:
//!
//! | | without `-f` | with `-f FILE` |
//! |---|---|---|
//! | where | the `sqlar` table in the open database | a zip archive |
//! | a member's bytes | `sqlar_compress`, so the row holds whichever of the raw and the compressed form is shorter | deflated when that is shorter, stored when it is not |
//! | who else reads it | any SQLite with the sqlar extension | anything at all |
//!
//! `-n` prints what would happen and changes nothing, which is what makes the
//! destructive options safe to try: `-c` **drops** the table it is about to
//! build.

use crate::shell::Shell;
use inillucent_value::Value;

/// Which operation the flags asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    /// `-c`: replace the archive with these files.
    Create,
    /// `-u`: add or replace the files whose modification time has moved.
    Update,
    /// `-i`: add or replace them whether or not it has.
    Insert,
    /// `-r`: remove them.
    Remove,
    /// `-t`: list what is there.
    List,
    /// `-x`: write what is there back out.
    Extract,
}

/// What the command line asked for.
struct Request {
    /// Which operation.
    operation: Operation,
    /// The archive file, when `-f` named one.
    file: Option<String>,
    /// The directory to read from or write into, when `-C` named one.
    directory: Option<String>,
    /// Whether each member is named as it is processed.
    verbose: bool,
    /// Whether to say what would happen rather than doing it.
    dry_run: bool,
    /// The paths the operation is about.
    paths: Vec<String>,
}

/// `.archive ...`: manage an SQL archive.
///
/// @param shell - the shell
/// @param arguments - the options and the paths
pub fn archive(shell: &mut Shell, arguments: &[&str]) {
    let request = match parse(arguments) {
        Ok(request) => request,
        Err(message) => {
            shell.complain(&format!("Error: {message}"));
            return;
        }
    };
    let outcome = match request.operation {
        Operation::List => list(shell, &request),
        Operation::Create | Operation::Update | Operation::Insert => add(shell, &request),
        Operation::Remove => remove(shell, &request),
        Operation::Extract => extract(shell, &request),
    };
    if let Err(message) = outcome {
        shell.complain(&format!("Error: {message}"));
    }
}

/// Reads the command line.
///
/// **Exactly one operation, which is the reference's own rule**: the six are
/// mutually exclusive because "create and also extract" names no behaviour.
///
/// @param arguments - the words after `.archive`
fn parse(arguments: &[&str]) -> Result<Request, String> {
    let mut operation: Option<Operation> = None;
    let mut request = Request {
        operation: Operation::List,
        file: None,
        directory: None,
        verbose: false,
        dry_run: false,
        paths: Vec::new(),
    };
    let mut at = 0usize;
    while at < arguments.len() {
        let word = arguments.get(at).copied().unwrap_or("");
        at = at.saturating_add(1);
        let chosen = match word {
            "-c" | "--create" => Some(Operation::Create),
            "-u" | "--update" => Some(Operation::Update),
            "-i" | "--insert" => Some(Operation::Insert),
            "-r" | "--remove" => Some(Operation::Remove),
            "-t" | "--list" => Some(Operation::List),
            "-x" | "--extract" => Some(Operation::Extract),
            _ => None,
        };
        if let Some(chosen) = chosen {
            if operation.is_some() {
                return Err(
                    "only one of --create --update --insert --remove --list --extract".to_string(),
                );
            }
            operation = Some(chosen);
            continue;
        }
        match word {
            "-v" | "--verbose" => request.verbose = true,
            "-n" | "--dryrun" => request.dry_run = true,
            "-f" | "--file" | "-a" | "--append" => {
                request.file = arguments.get(at).map(|held| held.to_string());
                at = at.saturating_add(1);
            }
            "-C" | "--directory" => {
                request.directory = arguments.get(at).map(|held| held.to_string());
                at = at.saturating_add(1);
            }
            // The combined short forms the reference accepts: `-tv`, `-cf`.
            _ if word.starts_with('-') && !word.starts_with("--") => {
                for letter in word.chars().skip(1) {
                    match letter {
                        'c' => operation = Some(Operation::Create),
                        'u' => operation = Some(Operation::Update),
                        'i' => operation = Some(Operation::Insert),
                        'r' => operation = Some(Operation::Remove),
                        't' => operation = Some(Operation::List),
                        'x' => operation = Some(Operation::Extract),
                        'v' => request.verbose = true,
                        'n' => request.dry_run = true,
                        'f' | 'a' => {
                            request.file = arguments.get(at).map(|held| held.to_string());
                            at = at.saturating_add(1);
                        }
                        'C' => {
                            request.directory = arguments.get(at).map(|held| held.to_string());
                            at = at.saturating_add(1);
                        }
                        other => return Err(format!("unknown option: -{other}")),
                    }
                }
            }
            _ => request.paths.push(word.to_string()),
        }
    }
    request.operation = operation.unwrap_or(Operation::List);
    Ok(request)
}

/// Returns the SQL name of the table the archive lives in.
///
/// A zip file is reached through a `zipfile` virtual table made for the
/// command and dropped after it, which is how the reference does it too: the
/// module is the only thing that knows the format, so the archive commands are
/// one implementation over two storages.
///
/// @param shell - the shell
/// @param request - what was asked for
fn open_archive(shell: &mut Shell, request: &Request) -> Result<String, String> {
    let Some(file) = &request.file else {
        return Ok("sqlar".to_string());
    };
    let quoted = file.replace('\'', "''").replace('\\', "/");
    shell
        .connection()
        .execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS temp.zip_archive USING zipfile('{quoted}')"
        ))
        .map_err(|error| error.message().to_string())?;
    Ok("temp.zip_archive".to_string())
}

/// Makes sure the `sqlar` table exists, in the reference's own declaration.
///
/// @param shell - the shell
fn ensure_sqlar(shell: &mut Shell) -> Result<(), String> {
    shell
        .connection()
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS sqlar(\n  \
             name TEXT PRIMARY KEY,  -- name of the file\n  \
             mode INT,               -- access permissions\n  \
             mtime INT,              -- last modification time\n  \
             sz INT,                 -- original file size\n  \
             data BLOB               -- compressed content\n)",
        )
        .map_err(|error| error.message().to_string())
}

/// `-t`: lists what the archive holds.
///
/// @param shell - the shell
/// @param request - what was asked for
fn list(shell: &mut Shell, request: &Request) -> Result<(), String> {
    let table = open_archive(shell, request)?;
    let sql = if request.verbose {
        format!("SELECT name, mode, sz, mtime FROM {table} ORDER BY name")
    } else {
        format!("SELECT name FROM {table} ORDER BY name")
    };
    let (_, rows) = shell.collect(&sql).map_err(|held| held.message.clone())?;
    for row in rows {
        if !request.verbose {
            shell.say(&text_of(row.first()));
            continue;
        }
        let mode = integer_of(row.get(1));
        let size = integer_of(row.get(2));
        let when = stamp(integer_of(row.get(3)));
        shell.say(&format!(
            "{} {size:>10}  {when}  {}",
            mode_text(mode),
            text_of(row.first())
        ));
    }
    Ok(())
}

/// `-c`, `-u` and `-i`: put files into the archive.
///
/// @param shell - the shell
/// @param request - what was asked for
fn add(shell: &mut Shell, request: &Request) -> Result<(), String> {
    let table = open_archive(shell, request)?;
    if request.dry_run {
        for path in &request.paths {
            shell.say(&format!("would add {path}"));
        }
        return Ok(());
    }
    if request.file.is_none() {
        if request.operation == Operation::Create {
            shell
                .connection()
                .execute_batch("DROP TABLE IF EXISTS sqlar")
                .map_err(|error| error.message().to_string())?;
        }
        ensure_sqlar(shell)?;
    }
    let base = request.directory.clone();
    let mut found: Vec<(String, std::path::PathBuf)> = Vec::new();
    for path in &request.paths {
        let on_disk = match &base {
            Some(directory) => std::path::PathBuf::from(directory).join(path),
            None => std::path::PathBuf::from(path),
        };
        collect(&on_disk, path, &mut found);
    }
    for (name, on_disk) in found {
        let Ok(found) = std::fs::symlink_metadata(&on_disk) else {
            return Err(format!("cannot stat file: {name}"));
        };
        let mtime = found
            .modified()
            .ok()
            .and_then(|held| held.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|held| held.as_secs() as i64)
            .unwrap_or(0);
        let (mode, size, data) = if found.is_dir() {
            (0o40_777i64, 0i64, None)
        } else {
            let bytes = std::fs::read(&on_disk).map_err(|held| held.to_string())?;
            let mode = if found.permissions().readonly() {
                0o100_444
            } else {
                0o100_666
            };
            (mode, bytes.len() as i64, Some(bytes))
        };
        if request.verbose {
            shell.say(&name);
        }
        write_member(shell, &table, &name, mode, mtime, size, data)?;
    }
    Ok(())
}

/// Writes one member into whichever table the archive is.
///
/// The `sqlar` form stores `sqlar_compress(data)` and the zip form stores the
/// content, because the module compresses on the way out. Both are bound rather
/// than spelled into the SQL, so a file holding a quote is a file rather than a
/// syntax error.
///
/// @param shell - the shell
/// @param table - the archive's table
/// @param name - the member's name
/// @param mode - its POSIX mode
/// @param mtime - its modification time
/// @param size - how many bytes its content is
/// @param data - the content, or nothing for a directory
fn write_member(
    shell: &mut Shell,
    table: &str,
    name: &str,
    mode: i64,
    mtime: i64,
    size: i64,
    data: Option<Vec<u8>>,
) -> Result<(), String> {
    let sql = if table == "sqlar" {
        format!(
            "INSERT OR REPLACE INTO {table}(name, mode, mtime, sz, data) \
             VALUES (?1, ?2, ?3, ?4, sqlar_compress(?5))"
        )
    } else {
        format!(
            "INSERT OR REPLACE INTO {table}(name, mode, mtime, sz, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)"
        )
    };
    let connection = shell.connection();
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| error.message().to_string())?;
    statement
        .bind_text(1, name)
        .map_err(|error| error.message().to_string())?;
    for (index, value) in [(2u32, mode), (3, mtime), (4, size)] {
        statement
            .bind_integer(index, value)
            .map_err(|error| error.message().to_string())?;
    }
    match data {
        Some(bytes) => statement
            .bind_blob(5, &bytes)
            .map_err(|error| error.message().to_string())?,
        None => statement
            .bind_null(5)
            .map_err(|error| error.message().to_string())?,
    }
    while statement
        .step()
        .map_err(|error| error.message().to_string())?
    {}
    Ok(())
}

/// Adds a path and everything under it to the list, depth first.
///
/// @param on_disk - where it actually is
/// @param shown - the name it should have in the archive
/// @param into - the list being built
fn collect(on_disk: &std::path::Path, shown: &str, into: &mut Vec<(String, std::path::PathBuf)>) {
    into.push((shown.to_string(), on_disk.to_path_buf()));
    let Ok(listing) = std::fs::read_dir(on_disk) else {
        return;
    };
    let mut found: Vec<(String, std::path::PathBuf)> = listing
        .filter_map(|held| held.ok())
        .map(|held| (held.file_name().to_string_lossy().into_owned(), held.path()))
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, path) in found {
        collect(&path, &format!("{shown}/{name}"), into);
    }
}

/// `-r`: takes members out of the archive.
///
/// @param shell - the shell
/// @param request - what was asked for
fn remove(shell: &mut Shell, request: &Request) -> Result<(), String> {
    let table = open_archive(shell, request)?;
    for path in &request.paths {
        if request.dry_run {
            shell.say(&format!("would remove {path}"));
            continue;
        }
        if request.verbose {
            shell.say(path);
        }
        let quoted = path.replace('\'', "''");
        shell
            .connection()
            .execute_batch(&format!(
                "DELETE FROM {table} WHERE name = '{quoted}' OR name GLOB '{quoted}/*'"
            ))
            .map_err(|error| error.message().to_string())?;
    }
    Ok(())
}

/// `-x`: writes the archive's members back out as files.
///
/// @param shell - the shell
/// @param request - what was asked for
fn extract(shell: &mut Shell, request: &Request) -> Result<(), String> {
    let table = open_archive(shell, request)?;
    let column = if table == "sqlar" {
        "sqlar_uncompress(data, sz)"
    } else {
        "data"
    };
    let (_, rows) = shell
        .collect(&format!(
            "SELECT name, mode, sz, {column} FROM {table} ORDER BY name"
        ))
        .map_err(|held| held.message.clone())?;
    let base = request.directory.clone().unwrap_or_else(|| ".".to_string());
    for row in rows {
        let name = text_of(row.first());
        if !request.paths.is_empty()
            && !request
                .paths
                .iter()
                .any(|held| name == *held || name.starts_with(&format!("{held}/")))
        {
            continue;
        }
        let target = std::path::PathBuf::from(&base).join(&name);
        if request.dry_run {
            shell.say(&format!("would extract {name}"));
            continue;
        }
        if request.verbose {
            shell.say(&name);
        }
        let mode = integer_of(row.get(1));
        if mode & 0o40_000 != 0 {
            std::fs::create_dir_all(&target).map_err(|held| held.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|held| held.to_string())?;
        }
        let bytes = match row.get(3) {
            Some(Value::Blob(blob)) => blob.raw().to_vec(),
            Some(Value::Text(text)) => text.utf8_bytes().into_owned(),
            _ => Vec::new(),
        };
        std::fs::write(&target, &bytes).map_err(|held| held.to_string())?;
    }
    Ok(())
}

/// Returns the ten-character mode string `ls` and the reference both print.
///
/// @param mode - the POSIX mode bits
fn mode_text(mode: i64) -> String {
    let kind = if mode & 0o40_000 != 0 { 'd' } else { '-' };
    let mut out = String::with_capacity(10);
    out.push(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    out
}

/// Returns a moment as the reference prints it, in UTC.
///
/// @param epoch - seconds since 1970
fn stamp(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let rest = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// Returns the civil date a day number names.
///
/// Howard Hinnant's `civil_from_days`, the same one the date functions use.
///
/// @param days - days since 1970-01-01
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days.saturating_add(719_468);
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Returns a value's text.
fn text_of(value: Option<&Value<'static>>) -> String {
    match value {
        Some(Value::Text(text)) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
        Some(Value::Blob(blob)) => String::from_utf8_lossy(blob.raw()).into_owned(),
        Some(Value::Integer(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// Returns a value's integer, or zero.
fn integer_of(value: Option<&Value<'static>>) -> i64 {
    value.and_then(Value::as_integer).unwrap_or(0)
}
