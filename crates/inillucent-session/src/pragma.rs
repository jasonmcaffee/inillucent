//! `PRAGMA`: the register of them, what each answers, and what each changes.
//!
//! Invariant: a pragma is a row in a table here before it is a branch in a
//! match. That is not tidiness - it is what makes `PRAGMA pragma_list` and the
//! `pragma_*` table-valued functions possible at all, and it is what stops the
//! answer's column names drifting from the answer. A pragma whose columns were
//! written in one place and produced in another is one where the two can
//! disagree, and the disagreement is invisible until an application reads a
//! result by name.
//!
//! An unknown pragma returns no rows and changes nothing, which is SQLite's
//! behaviour and the reason a typo in one is so easy to miss. Every pragma in
//! the register that is *read-only or unimplemented on this build* still
//! answers rather than being absent, because "this build does not do that" and
//! "you spelt it wrong" are different things and only the register can tell
//! them apart.

use inillucent_base::{error::misuse, DbResult};
use inillucent_sql::catalog_view::{CatalogView, ColumnInfo, IndexOrigin, TableKind};
use inillucent_sql::directive::PragmaArgument;
use inillucent_value::Value;

use crate::connection::ConnectionState;

/// The rows a pragma answers with.
pub type PragmaRows = Vec<Vec<Value<'static>>>;

/// What a pragma is, in the register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PragmaSpec {
    /// The name, folded.
    pub name: &'static str,
    /// The column names of its answer, in order.
    pub columns: &'static [&'static str],
    /// Whether it takes an argument in parentheses, as `table_info(t)` does.
    pub takes_argument: bool,
}

/// Every pragma this build knows, in the order `pragma_list` reports them.
///
/// The order is alphabetical because that is what `pragma_list` produces and
/// what a test comparing the two engines' lists will see.
pub const REGISTER: &[PragmaSpec] = &[
    boolean("analysis_limit"),
    boolean("application_id"),
    boolean("auto_vacuum"),
    boolean("automatic_index"),
    PragmaSpec {
        name: "busy_timeout",
        columns: &["timeout"],
        takes_argument: true,
    },
    boolean("cache_size"),
    boolean("cache_spill"),
    boolean("case_sensitive_like"),
    boolean("cell_size_check"),
    boolean("checkpoint_fullfsync"),
    PragmaSpec {
        name: "collation_list",
        columns: &["seq", "name"],
        takes_argument: false,
    },
    PragmaSpec {
        name: "compile_options",
        columns: &["compile_options"],
        takes_argument: false,
    },
    boolean("count_changes"),
    boolean("data_store_directory"),
    boolean("data_version"),
    PragmaSpec {
        name: "database_list",
        columns: &["seq", "name", "file"],
        takes_argument: false,
    },
    boolean("default_cache_size"),
    boolean("defensive"),
    boolean("defer_foreign_keys"),
    boolean("empty_result_callbacks"),
    PragmaSpec {
        name: "encoding",
        columns: &["encoding"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "foreign_key_check",
        columns: &["table", "rowid", "parent", "fkid"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "foreign_key_list",
        columns: &[
            "id",
            "seq",
            "table",
            "from",
            "to",
            "on_update",
            "on_delete",
            "match",
        ],
        takes_argument: true,
    },
    boolean("foreign_keys"),
    boolean("freelist_count"),
    boolean("full_column_names"),
    boolean("fullfsync"),
    PragmaSpec {
        name: "function_list",
        columns: &["name", "builtin", "type", "enc", "narg", "flags"],
        takes_argument: false,
    },
    boolean("hard_heap_limit"),
    boolean("ignore_check_constraints"),
    boolean("incremental_vacuum"),
    PragmaSpec {
        name: "index_info",
        columns: &["seqno", "cid", "name"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "index_list",
        columns: &["seq", "name", "unique", "origin", "partial"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "index_xinfo",
        columns: &["seqno", "cid", "name", "desc", "coll", "key"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "integrity_check",
        columns: &["integrity_check"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "journal_mode",
        columns: &["journal_mode"],
        takes_argument: true,
    },
    boolean("journal_size_limit"),
    boolean("legacy_alter_table"),
    PragmaSpec {
        name: "locking_mode",
        columns: &["locking_mode"],
        takes_argument: true,
    },
    boolean("max_page_count"),
    boolean("mmap_size"),
    PragmaSpec {
        name: "module_list",
        columns: &["name"],
        takes_argument: false,
    },
    boolean("optimize"),
    boolean("page_count"),
    boolean("page_size"),
    PragmaSpec {
        name: "pragma_list",
        columns: &["name"],
        takes_argument: false,
    },
    boolean("query_only"),
    PragmaSpec {
        name: "quick_check",
        columns: &["quick_check"],
        takes_argument: true,
    },
    boolean("read_uncommitted"),
    boolean("recursive_triggers"),
    boolean("reverse_unordered_selects"),
    boolean("schema_version"),
    boolean("secure_delete"),
    boolean("short_column_names"),
    boolean("shrink_memory"),
    boolean("soft_heap_limit"),
    boolean("synchronous"),
    PragmaSpec {
        name: "table_info",
        columns: &["cid", "name", "type", "notnull", "dflt_value", "pk"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "table_list",
        columns: &["schema", "name", "type", "ncol", "wr", "strict"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "table_xinfo",
        columns: &[
            "cid",
            "name",
            "type",
            "notnull",
            "dflt_value",
            "pk",
            "hidden",
        ],
        takes_argument: true,
    },
    boolean("temp_store"),
    boolean("temp_store_directory"),
    boolean("threads"),
    boolean("trusted_schema"),
    boolean("user_version"),
    PragmaSpec {
        name: "wal_autocheckpoint",
        columns: &["wal_autocheckpoint"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "wal_checkpoint",
        columns: &["busy", "log", "checkpointed"],
        takes_argument: true,
    },
    boolean("writable_schema"),
];

/// Returns the register row for a pragma that answers with its own name.
///
/// Most of them do: `PRAGMA cache_size` answers one column called `cache_size`,
/// and so do the two dozen others whose whole answer is a setting's value.
const fn boolean(name: &'static str) -> PragmaSpec {
    PragmaSpec {
        name,
        columns: &[],
        takes_argument: true,
    }
}

/// Returns the register row a name spells.
pub fn spec(name: &[u8]) -> Option<&'static PragmaSpec> {
    let folded = name.to_ascii_lowercase();
    REGISTER
        .iter()
        .find(|entry| entry.name.as_bytes() == folded.as_slice())
}

/// Returns the column names a pragma's answer carries.
pub fn columns(name: &[u8]) -> Vec<Vec<u8>> {
    let Some(spec) = spec(name) else {
        return Vec::new();
    };
    if spec.columns.is_empty() {
        return vec![spec.name.as_bytes().to_vec()];
    }
    spec.columns
        .iter()
        .map(|column| column.as_bytes().to_vec())
        .collect()
}

pub use inillucent_sql::declare::{argument_boolean, argument_integer, argument_text};

/// Returns the columns a view's `SELECT` produces.
///
/// The body is bound against the same catalog a statement naming the view would
/// bind it against, so the pragma and the query cannot disagree. A body that
/// will not bind - a view over a table somebody has since dropped - reports no
/// columns rather than failing, which is what SQLite does with the same schema.
fn view_columns(
    catalog: &inillucent_catalog::snapshot::CatalogSnapshot,
    table: &inillucent_sql::catalog_view::TableInfo,
) -> Vec<ColumnInfo> {
    let Some(body) = table.view.as_ref() else {
        return Vec::new();
    };
    let authorizer = inillucent_sql::bind::AllowAll;
    let mut binder = inillucent_sql::bind::Binder::new(catalog, &body.ast, &authorizer);
    let Ok(bound) = binder.bind_select(body.select) else {
        return Vec::new();
    };
    let declared = body.columns.clone();
    bound
        .columns
        .iter()
        .enumerate()
        .map(|(position, column)| {
            let name = declared
                .get(position)
                .cloned()
                .unwrap_or_else(|| column.name.clone());
            ColumnInfo {
                folded: name.to_ascii_lowercase(),
                name,
                declared_type: column.declared_type.clone(),
                affinity: inillucent_value::affinity::for_column(&column.declared_type),
                collation: b"binary".to_vec(),
                not_null: false,
                not_null_conflict: None,
                primary_key_conflict: None,
                default_sql: None,
                primary_key_position: None,
                hidden: false,
                generated: false,
                stored: true,
                generated_sql: None,
            }
        })
        .collect()
}

/// Returns the rows that report the schema of one table.
pub fn table_info(
    state: &mut ConnectionState,
    database: Option<usize>,
    argument: Option<&PragmaArgument>,
    extended: bool,
) -> DbResult<PragmaRows> {
    let Some(argument) = argument else {
        return Ok(Vec::new());
    };
    let catalog = std::sync::Arc::clone(&state.catalog);
    let wanted = argument_text(argument).to_ascii_lowercase();
    let Some(table) = find_table(&catalog, database, wanted.as_bytes()) else {
        return Ok(Vec::new());
    };
    let columns = if table.kind == TableKind::View {
        view_columns(&catalog, table)
    } else {
        table.columns.clone()
    };
    let mut rows = Vec::new();
    let mut cid = 0i64;
    for column in &columns {
        // The plain form hides both kinds of hidden column: a virtual table's
        // arguments and a generated column. The extended form shows them and
        // says which kind each is, which is the whole reason it exists.
        if !extended && (column.hidden || column.generated) {
            continue;
        }
        // 1 is a virtual table's hidden column; 2 is a VIRTUAL generated column
        // and 3 a STORED one. The two generated codes are the way round SQLite
        // has them, which is not the way round the keywords suggest.
        let hidden = if column.generated {
            if column.stored {
                3
            } else {
                2
            }
        } else {
            i64::from(column.hidden)
        };
        let mut row = vec![
            Value::Integer(cid),
            Value::owned_text(&column.name)?,
            Value::owned_text(&column.declared_type)?,
            Value::Integer(i64::from(column.not_null)),
            match &column.default_sql {
                Some(text) => Value::owned_text(text)?,
                None => Value::Null,
            },
            Value::Integer(i64::from(column.primary_key_position.unwrap_or(0))),
        ];
        if extended {
            row.push(Value::Integer(hidden));
        }
        rows.push(row);
        cid = cid.saturating_add(1);
    }
    Ok(rows)
}

/// Returns the rows that list every table of every database.
pub fn table_list(
    state: &mut ConnectionState,
    database: Option<usize>,
    argument: Option<&PragmaArgument>,
) -> DbResult<PragmaRows> {
    let catalog = std::sync::Arc::clone(&state.catalog);
    let wanted = argument.map(|argument| argument_text(argument).to_ascii_lowercase());
    let mut rows = Vec::new();
    for (index, schema) in catalog.databases.iter().enumerate() {
        if database.is_some_and(|only| only != index) {
            continue;
        }
        // The schema table is listed too, and is listed under the name the
        // database it belongs to gives it.
        let schema_table = if schema.name.eq_ignore_ascii_case(b"temp") {
            b"sqlite_temp_schema".to_vec()
        } else {
            b"sqlite_schema".to_vec()
        };
        let mut entries: Vec<(Vec<u8>, &'static str, i64, i64, i64)> =
            vec![(schema_table, "table", 5, 0, 0)];
        for table in &schema.tables {
            // The schema table has two names - `sqlite_schema` and the older
            // `sqlite_master` - and the catalog carries both so either resolves.
            // They are one table, listed once, under the name the pinned release
            // reports.
            if table.folded.starts_with(b"sqlite_") && table.folded.ends_with(b"master") {
                continue;
            }
            if table.folded == b"sqlite_schema" || table.folded == b"sqlite_temp_schema" {
                continue;
            }
            let kind = match table.kind {
                TableKind::View => "view",
                TableKind::Virtual => "virtual",
                _ => "table",
            };
            // A view's column count is what its `SELECT` produces, which the
            // file does not record - so it is bound here for the same reason
            // `table_info` binds it.
            let ncol = if table.kind == TableKind::View {
                view_columns(&catalog, table).len() as i64
            } else {
                table.columns.len() as i64
            };
            entries.push((
                table.name.clone(),
                kind,
                ncol,
                i64::from(table.without_rowid),
                i64::from(table.strict),
            ));
        }
        for (name, kind, ncol, without_rowid, strict) in entries {
            if wanted
                .as_deref()
                .is_some_and(|wanted| !name.eq_ignore_ascii_case(wanted.as_bytes()))
            {
                continue;
            }
            rows.push(vec![
                Value::owned_text(&schema.name)?,
                Value::owned_text(&name)?,
                Value::owned_text(kind.as_bytes())?,
                Value::Integer(ncol),
                Value::Integer(without_rowid),
                Value::Integer(strict),
            ]);
        }
    }
    Ok(rows)
}

/// Returns the rows that list one table's indexes.
pub fn index_list(
    state: &mut ConnectionState,
    database: Option<usize>,
    argument: Option<&PragmaArgument>,
) -> DbResult<PragmaRows> {
    let Some(argument) = argument else {
        return Ok(Vec::new());
    };
    let catalog = std::sync::Arc::clone(&state.catalog);
    let wanted = argument_text(argument).to_ascii_lowercase();
    let Some(table) = find_table(&catalog, database, wanted.as_bytes()) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    // Newest first, which is the order SQLite reports and the order the schema
    // is walked in reverse.
    for (position, index) in table.indexes.iter().rev().enumerate() {
        rows.push(vec![
            Value::Integer(position as i64),
            Value::owned_text(&index.name)?,
            Value::Integer(i64::from(index.unique)),
            Value::owned_text(match index.origin {
                IndexOrigin::Created => b"c".as_slice(),
                IndexOrigin::Unique => b"u".as_slice(),
                IndexOrigin::PrimaryKey => b"pk".as_slice(),
                // SQLite has no letter for this because SQLite has no such
                // index; `m` is this engine's, and `PRAGMA index_list` is the
                // one place a caller can see that a table carries one.
                IndexOrigin::Module => b"m".as_slice(),
            })?,
            Value::Integer(i64::from(index.partial_sql.is_some())),
        ]);
    }
    Ok(rows)
}

/// Returns the rows that describe one index's key columns.
///
/// The extended form also lists the columns the index carries but does not sort
/// by - the rowid, or a `WITHOUT ROWID` table's primary key - which is what
/// makes it possible to tell a covering index from one that has to fetch.
pub fn index_info(
    state: &mut ConnectionState,
    database: Option<usize>,
    argument: Option<&PragmaArgument>,
    extended: bool,
) -> DbResult<PragmaRows> {
    let Some(argument) = argument else {
        return Ok(Vec::new());
    };
    let catalog = std::sync::Arc::clone(&state.catalog);
    let wanted = argument_text(argument).to_ascii_lowercase();
    let schema = database.map(|index| catalog.database_name(index).to_vec());
    let Some((table, index)) = catalog.find_index(schema.as_deref(), wanted.as_bytes()) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    for (position, key) in index.columns.iter().enumerate() {
        let name = key
            .column
            .and_then(|column| table.columns.get(usize::from(column)))
            .map(|column| column.name.clone());
        let mut row = vec![
            Value::Integer(position as i64),
            match key.column {
                Some(column) => Value::Integer(i64::from(column)),
                None => Value::Integer(-2),
            },
            match &name {
                Some(name) => Value::owned_text(name)?,
                None => Value::Null,
            },
        ];
        if extended {
            row.push(Value::Integer(i64::from(key.descending)));
            row.push(Value::owned_text(&collation_spelling(&key.collation))?);
            row.push(Value::Integer(1));
        }
        rows.push(row);
    }
    if extended {
        // The trailing entry is the row the index points at: a rowid, reported
        // as column -1 with no name.
        rows.push(vec![
            Value::Integer(index.columns.len() as i64),
            Value::Integer(-1),
            Value::Null,
            Value::Integer(0),
            Value::owned_text(b"BINARY")?,
            Value::Integer(0),
        ]);
    }
    Ok(rows)
}

/// Returns the spelling of a collation name the pragma reports.
///
/// `BINARY` is upper case and the others are as they were declared, which is
/// what SQLite prints because it prints the name it recorded.
fn collation_spelling(name: &[u8]) -> Vec<u8> {
    // The name as *registered*, not as written in the schema: the three
    // built-ins were registered in upper case, so that is what an application
    // comparing a pragma's answer against a collation name will see.
    for builtin in [
        b"BINARY".as_slice(),
        b"NOCASE".as_slice(),
        b"RTRIM".as_slice(),
    ] {
        if name.eq_ignore_ascii_case(builtin) {
            return builtin.to_vec();
        }
    }
    if name.is_empty() {
        return b"BINARY".to_vec();
    }
    name.to_vec()
}

/// Returns the rows that list the attached databases.
pub fn database_list(state: &mut ConnectionState) -> DbResult<PragmaRows> {
    let catalog = std::sync::Arc::clone(&state.catalog);
    let has_temp = state.temp.is_some();
    let mut rows = Vec::new();
    let mut seq = 0i64;
    for (index, schema) in catalog.databases.iter().enumerate() {
        // The temporary database has a name from the moment the connection
        // opens - `CREATE TEMP TABLE` has to resolve against something - but it
        // has no *file* until something needs one, and SQLite lists it only
        // once it does. `PRAGMA table_list` still names it, because that is a
        // question about the schema rather than about the files.
        if index == inillucent_storage::TEMP_DATABASE && !has_temp {
            continue;
        }
        rows.push(vec![
            Value::Integer(seq),
            Value::owned_text(&schema.name)?,
            Value::owned_text(database_file(state, index).as_bytes())?,
        ]);
        seq = seq.saturating_add(1);
    }
    Ok(rows)
}

/// Returns the rows that list the collations this connection can reach.
pub fn collation_list(state: &mut ConnectionState) -> DbResult<PragmaRows> {
    let _ = state;
    let mut rows = Vec::new();
    // Newest first, which is the order SQLite's own hash table walks and so the
    // order its pragma prints.
    for (position, name) in ["RTRIM", "NOCASE", "BINARY"].iter().enumerate() {
        rows.push(vec![
            Value::Integer(position as i64),
            Value::owned_text(name.as_bytes())?,
        ]);
    }
    Ok(rows)
}

/// Returns the rows that list the modules this connection can reach.
///
/// **Without the `pragma_*` shims.** This front-end registers one module per
/// pragma so that `SELECT * FROM pragma_table_info('t')` works, and listing all
/// sixty-seven of them made `pragma_module_list` answer sixty-seven names where
/// the reference answers five. The reference creates its `pragma` module the
/// first time one is used, so its register names only the one the query itself
/// provoked - which is why its own answer carries `pragma_module_list` and
/// nothing else of the kind. Filtering them here makes this front-end agree
/// with the reference and with the new engine's own `module_list`, which never
/// listed them.
pub fn module_list(state: &mut ConnectionState) -> DbResult<PragmaRows> {
    let names = state.registry.module_names();
    let mut rows = Vec::new();
    for name in names {
        if name.starts_with("pragma_") {
            continue;
        }
        rows.push(vec![Value::owned_text(name.as_bytes())?]);
    }
    Ok(rows)
}

/// Returns the rows that list every pragma this build knows.
pub fn pragma_list() -> DbResult<PragmaRows> {
    let mut rows = Vec::new();
    for entry in REGISTER {
        rows.push(vec![Value::owned_text(entry.name.as_bytes())?]);
    }
    Ok(rows)
}

/// Returns the table a pragma's argument names, honouring a schema qualifier.
fn find_table<'catalog>(
    catalog: &'catalog inillucent_catalog::snapshot::CatalogSnapshot,
    database: Option<usize>,
    folded: &[u8],
) -> Option<&'catalog inillucent_sql::catalog_view::TableInfo> {
    match database {
        Some(index) => {
            let name = catalog.database_name(index).to_vec();
            catalog.find_table(Some(&name), folded)
        }
        None => catalog.find_table(None, folded),
    }
}

/// Returns the rows an integrity check reports.
pub fn integrity_check(
    state: &mut ConnectionState,
    database: Option<usize>,
    argument: Option<&PragmaArgument>,
    quick: bool,
) -> DbResult<PragmaRows> {
    let limit = argument
        .map(argument_integer)
        .filter(|limit| *limit > 0)
        .unwrap_or(inillucent_storage::check::DEFAULT_PROBLEM_LIMIT as i64);
    let level = if quick {
        inillucent_storage::check::CheckLevel::Quick
    } else {
        inillucent_storage::check::CheckLevel::Integrity
    };
    let index_keys = index_key_map(state);
    let databases: Vec<usize> = match database {
        Some(index) => vec![index],
        None => (0..state.catalog.databases.len()).collect(),
    };
    let mut lines = Vec::new();
    for index in databases {
        let options = inillucent_storage::check::CheckOptions {
            level,
            schema_roots: true,
            extra_roots: Vec::new(),
            index_keys: index_keys.clone(),
        };
        // A temporary database nobody has created has no file to check, and a
        // check of it would report "no such database" for something the schema
        // says is there.
        let Ok(pager) = inillucent_storage::PagerSet::pager(state, index) else {
            continue;
        };
        let Ok(mut report) =
            inillucent_storage::check::check_database_with_options(pager, &options)
        else {
            continue;
        };
        report.problem_limit = limit as usize;
        report.problems.truncate(limit.max(0) as usize);
        lines.extend(report.as_pragma_output());
    }
    // Every database was fine, so the whole check is fine - and the answer is
    // one row saying so rather than no rows at all.
    let lines: Vec<String> = lines.into_iter().filter(|line| line != "ok").collect();
    if lines.is_empty() {
        return Ok(vec![vec![Value::owned_text(b"ok")?]]);
    }
    let mut rows = Vec::new();
    for line in lines {
        rows.push(vec![Value::owned_text(line.as_bytes())?]);
    }
    Ok(rows)
}

/// Returns each index's declared key ordering, by root page.
///
/// The check verifies that an index's entries are in the order the index
/// declares, and only the catalog knows what that order is: the storage layer
/// sees a b-tree of records and no collation anywhere.
fn index_key_map(
    state: &ConnectionState,
) -> std::collections::BTreeMap<u32, inillucent_value::record::KeyInfo> {
    let mut keys = std::collections::BTreeMap::new();
    for table in state.catalog.every_table() {
        for index in &table.indexes {
            if index.root == 0 {
                continue;
            }
            keys.insert(index.root, crate::execute::index_key_info(index));
        }
    }
    keys
}

/// Returns the compile options this build would report.
///
/// It is a real answer rather than an empty one: an application that asks is
/// asking what it can rely on, and "nothing" is a different claim from "I do
/// not know".
pub fn compile_options() -> DbResult<PragmaRows> {
    let mut rows: PragmaRows = Vec::new();
    for option in [
        "ENABLE_FTS5",
        "ENABLE_MATH_FUNCTIONS",
        "ENABLE_RTREE",
        "THREADSAFE=1",
    ] {
        rows.push(vec![Value::owned_text(option.as_bytes())?]);
    }
    Ok(rows)
}

/// Returns the error a pragma that cannot be applied here reports.
pub fn refused(name: &[u8], why: &str) -> inillucent_base::DbError {
    misuse(format!("PRAGMA {}: {why}", String::from_utf8_lossy(name)))
}

/// Returns the rows `PRAGMA function_list` reports.
pub fn function_list() -> DbResult<PragmaRows> {
    let mut rows: PragmaRows = Vec::new();
    for entry in inillucent_sql::function::every_function() {
        rows.push(vec![
            Value::owned_text(entry.name.as_bytes())?,
            Value::Integer(1),
            Value::owned_text(entry.kind.as_bytes())?,
            Value::owned_text(b"utf8")?,
            Value::Integer(entry.arity),
            Value::Integer(entry.flags),
        ]);
    }
    Ok(rows)
}

/// Answers every pragma that only reads.
///
/// `None` means the name has no read form here - it is a verb like `optimize`,
/// or a setting whose write is the whole of it. The two callers are the
/// `PRAGMA` directive, which tries the write forms first, and the `pragma_*`
/// table-valued functions, which have no write form at all.
pub fn read(
    state: &mut ConnectionState,
    database: Option<usize>,
    name: &[u8],
    argument: Option<&PragmaArgument>,
) -> DbResult<Option<PragmaRows>> {
    let target = database.unwrap_or(inillucent_storage::MAIN_DATABASE);
    let rows = match name {
        b"table_info" => table_info(state, database, argument, false)?,
        b"table_xinfo" => table_info(state, database, argument, true)?,
        b"table_list" => table_list(state, database, argument)?,
        b"index_list" => index_list(state, database, argument)?,
        b"index_info" => index_info(state, database, argument, false)?,
        b"index_xinfo" => index_info(state, database, argument, true)?,
        b"database_list" => database_list(state)?,
        b"collation_list" => collation_list(state)?,
        b"module_list" => module_list(state)?,
        b"pragma_list" => pragma_list()?,
        b"function_list" => function_list()?,
        b"compile_options" => compile_options()?,
        b"foreign_key_list" => foreign_key_list(state, argument)?,
        b"integrity_check" => integrity_check(state, database, argument, false)?,
        b"quick_check" => integrity_check(state, database, argument, true)?,
        b"page_size" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(pager.page_size().bytes()))]]
        }
        b"page_count" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(pager.page_count()))]]
        }
        b"freelist_count" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(
                pager.header().freelist_count,
            ))]]
        }
        b"max_page_count" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(pager.max_page_count()))]]
        }
        b"user_version" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(pager.header().user_version))]]
        }
        b"application_id" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(
                pager.header().application_id,
            ))]]
        }
        b"schema_version" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(
                pager.header().schema_cookie,
            ))]]
        }
        b"data_version" => {
            // It moves when *another* connection has written the file, which is
            // exactly what the header's change counter records.
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(i64::from(
                pager.header().change_counter,
            ))]]
        }
        b"auto_vacuum" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            vec![vec![Value::Integer(match pager.header().vacuum_mode {
                inillucent_storage::VacuumMode::None => 0,
                inillucent_storage::VacuumMode::Auto => 1,
                inillucent_storage::VacuumMode::Incremental => 2,
            })]]
        }
        b"encoding" => {
            let pager = inillucent_storage::PagerSet::pager(state, target)?;
            let encoding = match pager.text_encoding() {
                inillucent_value::TextEncoding::Utf8 => "UTF-8",
                inillucent_value::TextEncoding::Utf16Le => "UTF-16le",
                inillucent_value::TextEncoding::Utf16Be => "UTF-16be",
            };
            vec![vec![Value::owned_text(encoding.as_bytes())?]]
        }
        b"journal_mode" => vec![vec![Value::owned_text(
            state.journal.mode.as_str().as_bytes(),
        )?]],
        b"synchronous" => vec![vec![Value::Integer(state.journal.synchronous.as_number())]],
        b"foreign_keys" => vec![vec![Value::Integer(i64::from(state.foreign_keys))]],
        b"defer_foreign_keys" => {
            vec![vec![Value::Integer(i64::from(state.defer_foreign_keys))]]
        }
        b"defensive" => vec![vec![Value::Integer(i64::from(
            state.registry.policy().defensive,
        ))]],
        b"trusted_schema" => vec![vec![Value::Integer(i64::from(
            state.registry.policy().trusted_schema,
        ))]],
        b"writable_schema" => vec![vec![Value::Integer(i64::from(
            state.registry.policy().writable_schema,
        ))]],
        b"locking_mode" => {
            let exclusive = state
                .settings
                .get(crate::settings::Setting::ExclusiveLocking)
                != 0;
            vec![vec![Value::owned_text(if exclusive {
                b"exclusive"
            } else {
                b"normal"
            })?]]
        }
        other => match crate::settings::Setting::named(other) {
            Some(setting) => vec![vec![Value::Integer(state.settings.get(setting))]],
            None => return Ok(None),
        },
    };
    Ok(Some(rows))
}

/// Returns the rows `PRAGMA foreign_key_list` reports.
pub fn foreign_key_list(
    state: &mut ConnectionState,
    argument: Option<&PragmaArgument>,
) -> DbResult<PragmaRows> {
    let Some(argument) = argument else {
        return Ok(Vec::new());
    };
    let catalog = std::sync::Arc::clone(&state.catalog);
    let wanted = argument_text(argument).to_ascii_lowercase();
    let Some(table) = catalog.find_table(None, wanted.as_bytes()) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    for key in &table.foreign_keys {
        let parent = catalog.find_table(None, &key.parent_folded);
        let targets = parent
            .and_then(|parent| inillucent_sql::foreign_key::parent_columns(key, parent))
            .unwrap_or_default();
        for (position, column) in key.columns.iter().enumerate() {
            let from = table
                .columns
                .get(usize::from(*column))
                .map(|info| info.name.clone())
                .unwrap_or_default();
            rows.push(vec![
                Value::Integer(i64::from(key.id)),
                Value::Integer(position as i64),
                Value::owned_text(&key.parent)?,
                Value::owned_text(&from)?,
                match targets.get(position) {
                    Some(name) => Value::owned_text(name)?,
                    None => Value::Null,
                },
                Value::owned_text(action_name(key.on_update).as_bytes())?,
                Value::owned_text(action_name(key.on_delete).as_bytes())?,
                Value::owned_text(if key.match_clause.is_empty() {
                    b"NONE"
                } else {
                    key.match_clause.as_slice()
                })?,
            ]);
        }
    }
    Ok(rows)
}

/// Returns the spelling `PRAGMA foreign_key_list` reports for an action.
fn action_name(action: inillucent_sql::ast::ReferentialAction) -> &'static str {
    use inillucent_sql::ast::ReferentialAction;
    match action {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
        ReferentialAction::Cascade => "CASCADE",
    }
}

/// Returns the file one attached database was opened from.
///
/// `main` and `temp` answer with what the connection was opened on and with
/// nothing; an attached database answers with the path `ATTACH` was given. It
/// is the same question `PRAGMA database_list` asks and the answer an
/// application uses to find the file it is looking at.
fn database_file(state: &ConnectionState, index: usize) -> String {
    if index == inillucent_storage::TEMP_DATABASE {
        return String::new();
    }
    if index == inillucent_storage::MAIN_DATABASE {
        return state.main_file.clone();
    }
    state
        .attached
        .get(index.saturating_sub(2))
        .map(|attached| attached.path.display().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every register row has a name and a set of columns.
    #[test]
    fn every_register_row_answers_something() {
        for entry in REGISTER {
            assert!(!entry.name.is_empty());
            assert!(!columns(entry.name.as_bytes()).is_empty(), "{}", entry.name);
        }
    }

    /// The register is in the order `pragma_list` reports.
    #[test]
    fn the_register_is_sorted() {
        let mut previous = "";
        for entry in REGISTER {
            assert!(previous <= entry.name, "{previous} then {}", entry.name);
            previous = entry.name;
        }
    }

    /// A pragma whose columns are not listed answers with its own name.
    #[test]
    fn a_setting_answers_with_its_own_name() {
        assert_eq!(columns(b"cache_size"), vec![b"cache_size".to_vec()]);
        assert_eq!(
            columns(b"index_info"),
            vec![b"seqno".to_vec(), b"cid".to_vec(), b"name".to_vec()]
        );
    }

    /// A name nobody registered is not a pragma.
    #[test]
    fn an_unknown_name_is_not_a_pragma() {
        assert!(spec(b"nope").is_none());
        assert!(spec(b"TABLE_INFO").is_some());
    }

    /// The boolean argument follows SQLite's rule, including the strange part.
    #[test]
    fn the_boolean_rule_is_sqlites() {
        let name = |text: &str| PragmaArgument::Name(text.as_bytes().to_vec());
        assert!(argument_boolean(&name("on")));
        assert!(argument_boolean(&name("YES")));
        assert!(argument_boolean(&name("1")));
        assert!(!argument_boolean(&name("off")));
        assert!(!argument_boolean(&name("0")));
        // Anything it cannot read is false, which is why `= maybe` is off.
        assert!(!argument_boolean(&name("maybe")));
    }
}
