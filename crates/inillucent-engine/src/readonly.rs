//! What a read only connection is allowed to run.
//!
//! Invariant: **a statement is admitted by its class, not by the text of an
//! error somebody else's code happened to produce.** The check this replaces
//! asked the engine to `EXPLAIN` the statement and refused only when the
//! message contained "not a read-only statement" - and `compile_explain`
//! describes a plan for `SELECT`, `UPDATE` and `DELETE` and sends everything
//! else, `INSERT` included, to a describer that answers `Ok`. So INSERT,
//! UPDATE, DELETE, a write pragma, `ATTACH` and `VACUUM INTO` all ran and
//! persisted through a read only CLI, a read only MCP server and the read only
//! driver (task-1979, section 5.2).
//!
//! **This is one of two layers and it is the one that gives a good message.**
//! The other is the commit path, which refuses a write on a connection opened
//! read only whatever reached it - see `ImportedDatabase::refuse_a_read_only_write`.
//! A filter on the command surface is bypassable by the next verb somebody
//! adds; the commit path is the one place every write passes.

use inillucent_sql::parser::{classify_statement, StatementClass};

/// The pragmas a read only connection may set.
///
/// **What they have in common is that setting one writes nothing to the
/// file.** Each changes this connection's own behaviour, this process's
/// memory, or nothing at all, so a caller that opened a database read only can
/// still set its cache size, its busy timeout and its foreign key enforcement -
/// which is what SQLite lets a read only connection do, and what an application
/// that opens a reporting connection expects.
///
/// Reading a pragma is always allowed and is not decided by this list: a
/// `PRAGMA` with no argument reports and never writes, whichever name it
/// carries.
pub const CONNECTION_PRAGMAS: &[&str] = &[
    "analysis_limit",
    "automatic_index",
    "busy_timeout",
    "cache_size",
    "cache_spill",
    "case_sensitive_like",
    "cell_size_check",
    "checkpoint_fullfsync",
    "collation_list",
    "compile_options",
    "count_changes",
    "data_store_directory",
    "data_version",
    "database_list",
    "defensive",
    "defer_foreign_keys",
    "empty_result_callbacks",
    "foreign_key_check",
    "foreign_key_list",
    "foreign_keys",
    "freelist_count",
    "full_column_names",
    "fullfsync",
    "function_list",
    "hard_heap_limit",
    "ignore_check_constraints",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "journal_size_limit",
    "legacy_alter_table",
    "locking_mode",
    "mmap_size",
    "module_list",
    "page_count",
    "pragma_list",
    "query_only",
    "quick_check",
    "read_uncommitted",
    "recursive_triggers",
    "reverse_unordered_selects",
    "secure_delete",
    "short_column_names",
    "shrink_memory",
    "soft_heap_limit",
    "synchronous",
    "table_info",
    "table_list",
    "table_xinfo",
    "temp_store",
    "temp_store_directory",
    "threads",
    "trusted_schema",
    "wal_autocheckpoint",
    "writable_schema",
];

/// The pragmas a read only connection may not set.
///
/// **Each one reaches the file.** Six write a field of the header
/// (`application_id`, `schema_version`, `user_version`, `encoding`,
/// `page_size`, `max_page_count`), two change the file's layout (`auto_vacuum`,
/// `default_cache_size`), and four make the engine write pages
/// (`incremental_vacuum`, `journal_mode`, `optimize`, `wal_checkpoint`).
///
/// It is stated rather than derived so that a pragma added to the register and
/// to neither list fails `crates/inillucent-compat/tests/engine/readonly_pragmas.rs`
/// instead of quietly defaulting to whichever answer the code happens to give.
pub const FILE_PRAGMAS: &[&str] = &[
    "application_id",
    "auto_vacuum",
    "default_cache_size",
    "encoding",
    "incremental_vacuum",
    "journal_mode",
    "max_page_count",
    "optimize",
    "page_size",
    "schema_version",
    "user_version",
    "wal_checkpoint",
];

/// Returns whether a read only connection may run this statement.
///
/// @param sql - the statement, as the caller wrote it
pub fn admits(sql: &str) -> bool {
    match classify_statement(sql.as_bytes()) {
        // `EXPLAIN` classifies as read only whatever it wraps, which is right:
        // it never runs the statement.
        StatementClass::ReadOnly | StatementClass::Empty => true,
        StatementClass::Pragma => admits_pragma(sql),
        // **`TransactionControl` is refused, and that is deliberate.** A
        // `BEGIN` on a connection that can run nothing inside it is a
        // transaction that can only be rolled back, and letting it open holds
        // the file for a caller that has no use for it.
        StatementClass::Write
        | StatementClass::SchemaChange
        | StatementClass::TransactionControl
        | StatementClass::Unknown => false,
    }
}

/// The pragmas that write even with no argument.
///
/// **The three that are verbs rather than settings.** `PRAGMA optimize` runs
/// `ANALYZE` and rewrites the statistics, `PRAGMA wal_checkpoint` writes the
/// log's pages into the file, and `PRAGMA incremental_vacuum` moves pages and
/// truncates - so the "no argument means it only reports" rule below does not
/// hold for them and they are refused by name. Every one is in
/// [`FILE_PRAGMAS`] too.
pub const ALWAYS_WRITING_PRAGMAS: &[&str] = &["incremental_vacuum", "optimize", "wal_checkpoint"];

/// Returns whether a read only connection may run this `PRAGMA`.
///
/// A pragma with no argument reports and never writes, with the three
/// exceptions in [`ALWAYS_WRITING_PRAGMAS`]. One with an argument is a setting,
/// and is admitted only when the setting is this connection's own.
///
/// @param sql - the statement, which begins with `PRAGMA`
fn admits_pragma(sql: &str) -> bool {
    let Some((name, sets)) = pragma_shape(sql) else {
        // Not a shape this reader understands, so not one to admit.
        return false;
    };
    if ALWAYS_WRITING_PRAGMAS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&name))
    {
        return false;
    }
    if !sets {
        return true;
    }
    CONNECTION_PRAGMAS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&name))
}

/// Returns a pragma's name and whether it is being set.
///
/// The schema qualifier is dropped: `PRAGMA main.user_version = 7` sets the
/// same thing `PRAGMA user_version = 7` does, and a filter that read the
/// qualifier as the name would admit every pragma a caller chose to qualify.
///
/// @param sql - the statement, which begins with `PRAGMA`
fn pragma_shape(sql: &str) -> Option<(String, bool)> {
    let trimmed = sql.trim_start();
    let rest = trimmed
        .get(..6)
        .filter(|head| head.eq_ignore_ascii_case("pragma"))
        .and_then(|_| trimmed.get(6..))?;
    let rest = rest.trim_start();
    let end = rest
        .find(|letter: char| !letter.is_ascii_alphanumeric() && letter != '_' && letter != '.')
        .unwrap_or(rest.len());
    let named = rest.get(..end)?;
    let name = named.rsplit('.').next()?.to_string();
    if name.is_empty() {
        return None;
    }
    let after = rest.get(end..).unwrap_or("").trim_start();
    Some((name, after.starts_with('=') || after.starts_with('(')))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four classes a read only connection refuses.
    #[test]
    fn a_statement_that_writes_is_refused() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "  insert into t values (1)",
            "/* comment */ UPDATE t SET a = 1",
            "DELETE FROM t",
            "REPLACE INTO t VALUES (1)",
            "CREATE TABLE t(a)",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN b",
            "VACUUM INTO 'copy.rdb'",
            "ATTACH 'other.rdb' AS aux",
            "DETACH aux",
            "REINDEX",
            "ANALYZE",
            "BEGIN",
            "COMMIT",
            "WITH c AS (SELECT 1) INSERT INTO t SELECT * FROM c",
        ] {
            assert!(!admits(sql), "{sql} was admitted on a read only connection");
        }
    }

    /// A query is admitted however it is written.
    #[test]
    fn a_query_is_admitted() {
        for sql in [
            "SELECT 1",
            "  select * from t",
            "VALUES (1)",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "EXPLAIN SELECT 1",
            "EXPLAIN QUERY PLAN SELECT 1",
        ] {
            assert!(admits(sql), "{sql} was refused on a read only connection");
        }
    }

    /// Reading a pragma is admitted; setting one is admitted only when the
    /// setting is the connection's own.
    #[test]
    fn a_pragma_is_admitted_by_what_it_would_change() {
        for sql in [
            "PRAGMA user_version",
            "PRAGMA main.user_version",
            "PRAGMA journal_mode",
            "PRAGMA cache_size = -2000",
            "PRAGMA busy_timeout(500)",
            "PRAGMA table_info(t)",
            "PRAGMA integrity_check",
        ] {
            assert!(admits(sql), "{sql} was refused on a read only connection");
        }
        for sql in [
            "PRAGMA user_version = 7",
            "PRAGMA main.user_version = 7",
            "PRAGMA MAIN.USER_VERSION=7",
            "PRAGMA journal_mode = wal",
            "PRAGMA wal_checkpoint(TRUNCATE)",
            "PRAGMA wal_checkpoint",
            "PRAGMA optimize",
            "PRAGMA optimize(0x02)",
            "PRAGMA incremental_vacuum",
            "PRAGMA application_id = 3",
        ] {
            assert!(!admits(sql), "{sql} was admitted on a read only connection");
        }
    }

    /// The two lists do not overlap, and the third is inside the second.
    #[test]
    fn no_pragma_is_in_both_lists() {
        for name in CONNECTION_PRAGMAS {
            assert!(
                !FILE_PRAGMAS.iter().any(|other| other == name),
                "{name} is in both lists"
            );
        }
        for name in ALWAYS_WRITING_PRAGMAS {
            assert!(
                FILE_PRAGMAS.iter().any(|other| other == name),
                "{name} writes with no argument and is not among the file pragmas"
            );
        }
    }
}
